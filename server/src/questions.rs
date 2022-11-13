use std::collections::HashMap;

use super::{Backend, Local};
use aws_sdk_dynamodb::{
    error::BatchGetItemError,
    model::{AttributeValue, KeysAndAttributes},
    output::BatchGetItemOutput,
    types::SdkError,
};
use axum::{
    extract::{Extension, Path},
    response::AppendHeaders,
    Json,
};
use http::{
    header::{self, HeaderName},
    StatusCode,
};
use serde_json::Value;
use uuid::Uuid;

#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

impl Backend {
    pub(super) async fn questions(
        &self,
        qids: &[Uuid],
    ) -> Result<BatchGetItemOutput, SdkError<BatchGetItemError>> {
        match self {
            Self::Dynamo(dynamo) => {
                let keys = qids
                    .iter()
                    .map(|qid| {
                        HashMap::from_iter([(
                            String::from("id"),
                            AttributeValue::S(qid.to_string()),
                        )])
                    })
                    .collect();
                dynamo
                    .batch_get_item()
                    .request_items(
                        "questions",
                        KeysAndAttributes::builder()
                            .set_keys(Some(keys))
                            .projection_expression("id,#text,#when")
                            .expression_attribute_names("#text", "text")
                            .expression_attribute_names("#when", "when")
                            .build(),
                    )
                    .send()
                    .await
            }
            Self::Local(local) => {
                let mut local = local.lock().unwrap();
                let Local { questions, .. } = &mut *local;

                let unprocessed: Vec<_> = qids
                    .iter()
                    .filter(|qid| !questions.contains_key(qid))
                    .map(|qid| {
                        HashMap::from_iter([(
                            String::from("id"),
                            AttributeValue::S(qid.to_string()),
                        )])
                    })
                    .collect();
                let unprocessed = if unprocessed.is_empty() {
                    None
                } else {
                    Some(HashMap::from_iter([(
                        String::from("questions"),
                        KeysAndAttributes::builder()
                            .set_keys(Some(unprocessed))
                            .projection_expression("text,when")
                            .build(),
                    )]))
                };

                Ok(BatchGetItemOutput::builder()
                    .set_unprocessed_keys(unprocessed)
                    .set_responses(Some(HashMap::from_iter([(
                        String::from("questions"),
                        qids.iter()
                            .filter_map(|qid| {
                                Some(
                                    questions
                                        .get(qid)?
                                        .iter()
                                        .filter(|&(k, _)| matches!(*k, "id" | "text" | "when"))
                                        .map(|(k, v)| (k.to_string(), v.clone()))
                                        .collect(),
                                )
                            })
                            .collect(),
                    )])))
                    .build())
            }
        }
    }
}

pub(super) async fn questions(
    Path(qids): Path<String>,
    Extension(dynamo): Extension<Backend>,
) -> (
    AppendHeaders<HeaderName, &'static str, 1>,
    Result<Json<Value>, StatusCode>,
) {
    let qids: Vec<_> = match qids.split(',').map(Uuid::parse_str).collect() {
        Ok(v) => v,
        Err(e) => {
            warn!(%qids, error = %e, "got invalid uuid set");
            return (
                // a bad request will never become good
                AppendHeaders([(header::CACHE_CONTROL, "max-age=864001")]),
                Err(http::StatusCode::BAD_REQUEST),
            );
        }
    };
    match dynamo.questions(&qids).await {
        Ok(v) => {
            if v.responses().map_or(true, |r| r.is_empty()) {
                warn!(?qids, "no valid qids");
                return (
                    // it should be unlikely that someone fetches a question that hasn't been asked
                    // it's _possible_ that it happens and _then_ a question is assigned that uuid,
                    // but it too seems rare.
                    AppendHeaders([(header::CACHE_CONTROL, "max-age=600")]),
                    Err(http::StatusCode::NOT_FOUND),
                );
            }
            let r = v.responses().unwrap();
            let t = if let Some(t) = r.get("questions") {
                t
            } else {
                error!(?qids, ?v, "got non-empty non-questions response");
                return (
                    AppendHeaders([(header::CACHE_CONTROL, "no-cache")]),
                    Err(http::StatusCode::INTERNAL_SERVER_ERROR),
                );
            };

            let r = t
                .iter()
                .map(|q| {
                    let qid = q
                        .get("id")
                        .and_then(|v| v.as_s().ok())
                        .and_then(|v| Uuid::parse_str(v).ok());
                    let text = q.get("text").and_then(|v| v.as_s().ok());
                    let when = q
                        .get("when")
                        .and_then(|v| v.as_n().ok())
                        .and_then(|v| v.parse::<usize>().ok());
                    match (qid, text, when) {
                        (Some(qid), Some(text), Some(when)) => Ok((
                            qid.to_string(),
                            serde_json::json!({
                                "text": text,
                                "when": when,
                            }),
                        )),
                        _ => {
                            error!(?qids, ?q, "bad data types for id/text/when");
                            Err(StatusCode::INTERNAL_SERVER_ERROR)
                        }
                    }
                })
                .collect::<Result<_, _>>()
                .map(Json);
            if r.is_ok() {
                (
                    AppendHeaders([(header::CACHE_CONTROL, "max-age=864001")]),
                    r,
                )
            } else {
                (AppendHeaders([(header::CACHE_CONTROL, "no-cache")]), r)
            }
        }
        Err(e) => {
            error!(?qids, error = %e, "dynamodb question request failed");
            (
                AppendHeaders([(header::CACHE_CONTROL, "no-cache")]),
                Err(http::StatusCode::INTERNAL_SERVER_ERROR),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn inner(backend: Backend) {
        let eid = Uuid::new_v4();
        let secret = "cargo-test";
        let _ = backend.new(&eid, secret).await.unwrap();
        let qid1 = Uuid::new_v4();
        let qid1_v = AttributeValue::S(qid1.to_string());
        backend.ask(&eid, &qid1, "hello world").await.unwrap();
        let qid2 = Uuid::new_v4();
        let qid2_v = AttributeValue::S(qid2.to_string());
        backend.ask(&eid, &qid2, "hello moon").await.unwrap();

        let qids = backend
            .questions(&[qid1.clone(), qid2.clone()])
            .await
            .unwrap();

        let qids = qids.responses().unwrap();
        let qids = &qids["questions"];
        let q1 = qids.iter().find(|q| q["id"] == qid1_v).unwrap();
        assert_eq!(q1["id"], AttributeValue::S(qid1.to_string()));
        assert_eq!(q1["text"], AttributeValue::S("hello world".to_string()));
        assert!(matches!(q1["when"], AttributeValue::N(_)));
        let q2 = qids.iter().find(|q| q["id"] == qid2_v).unwrap();
        assert_eq!(q2["id"], AttributeValue::S(qid2.to_string()));
        assert_eq!(q2["text"], AttributeValue::S("hello moon".to_string()));
        assert!(matches!(q2["when"], AttributeValue::N(_)));

        backend.delete(&eid).await;
    }

    #[tokio::test]
    async fn local() {
        inner(Backend::local().await).await;
    }

    #[tokio::test]
    #[ignore]
    async fn dynamodb() {
        inner(Backend::dynamo().await).await;
    }
}
