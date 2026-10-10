//! crates/leave の trait の Workers での実装: R2 の binding (`LEAVE_R2`)・一番星 Worker の Service Binding (`ICHIBAN_DB`)・
//! LINE WORKS への送信 (auth-worker の Service Binding `AUTH_WORKER`)。
//!
//! 値と future は JS の値で `Send` でないので、`worker::send` の `SendWrapper`・`SendFuture` で包む (Workers は単一 thread)。
//! エラーは段の名前だけに落とす (**key・ランタイムの生のエラー文はログに出さない**)。
//!
//! このファイルは wasm 専用で、CI のテストと coverage の gate の外 (確かめるのは staging / 本番の実物)。

use futures_util::future::BoxFuture;
use leave::cron::{send_body, Notifier, NotifyError, LINEWORKS_SEND_URL};
use leave::ichiban::{IchibanApi, IchibanError, IchibanResponse};
use leave::store::{PdfStore, StoreError};
use uuid::Uuid;
use wasm_bindgen::JsValue;
use worker::send::{SendFuture, SendWrapper};
use worker::{Bucket, Fetcher, Headers, Method, Request, RequestInit};

/// R2 の bucket (binding `LEAVE_R2`)
pub struct R2Pdfs(SendWrapper<Bucket>);

impl R2Pdfs {
    pub fn new(bucket: Bucket) -> Self {
        Self(SendWrapper::new(bucket))
    }
}

impl PdfStore for R2Pdfs {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, StoreError>> {
        Box::pin(SendFuture::new(async move {
            let object = self.0.get(key).execute().await;
            let Some(object) = object.map_err(|_| StoreError("get"))? else {
                return Ok(None);
            };
            let body = object.body().ok_or(StoreError("get body"))?;
            let bytes = body.bytes().await.map_err(|_| StoreError("read body"))?;
            Ok(Some(bytes))
        }))
    }
}

/// 一番星 Worker (binding `ICHIBAN_DB`)。URL の host は名目の値 (binding は host で経路が決まらない。
/// nuxt-ichibanboshi の `ichibanWorkerFetch` と同じ)
pub struct IchibanBinding(SendWrapper<Fetcher>);

const ICHIBAN_NOMINAL_ORIGIN: &str = "https://ichibanboshi-ichiban";
/// 一番星の応答の本文の上限 (社員ﾏｽﾀ全件でも十分に収まる大きさ)
const ICHIBAN_BODY_LIMIT: usize = 16 * 1024 * 1024;

impl IchibanBinding {
    pub fn new(fetcher: Fetcher) -> Self {
        Self(SendWrapper::new(fetcher))
    }
}

impl IchibanApi for IchibanBinding {
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<IchibanResponse, IchibanError>> {
        Box::pin(SendFuture::new(async move {
            let mut init = RequestInit::new();
            init.with_method(Method::Get);
            let req = Request::new_with_init(&format!("{ICHIBAN_NOMINAL_ORIGIN}{path}"), &init)
                .map_err(|_| IchibanError)?;
            req.headers()
                .set("Accept", "application/json")
                .map_err(|_| IchibanError)?;
            let resp = self.0.fetch_request(req).await.map_err(|_| IchibanError)?;
            let status = resp.status().as_u16();
            let body =
                axum::body::to_bytes(axum::body::Body::new(resp.into_body()), ICHIBAN_BODY_LIMIT)
                    .await
                    .map_err(|_| IchibanError)?;
            Ok(IchibanResponse {
                status,
                body: body.to_vec(),
            })
        }))
    }
}

/// auth-worker (Service Binding `AUTH_WORKER`) の内部 proxy 経由で LINE WORKS へ送る。
/// `X-Alc-Proxy-Secret` は Secrets Store の共有 secret (consumer worker の証明)。応答の本文は読まない
pub struct AuthWorkerNotifier {
    fetcher: SendWrapper<Fetcher>,
    secret: String,
}

impl AuthWorkerNotifier {
    pub fn new(fetcher: Fetcher, secret: String) -> Self {
        Self {
            fetcher: SendWrapper::new(fetcher),
            secret,
        }
    }
}

impl Notifier for AuthWorkerNotifier {
    fn send<'a>(
        &'a self,
        channel_id: Uuid,
        text: &'a str,
    ) -> BoxFuture<'a, Result<(), NotifyError>> {
        Box::pin(SendFuture::new(async move {
            let headers = Headers::new();
            headers
                .set("X-Alc-Proxy-Secret", &self.secret)
                .map_err(|_| NotifyError("headers"))?;
            headers
                .set("Content-Type", "application/json")
                .map_err(|_| NotifyError("headers"))?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post)
                .with_headers(headers)
                .with_body(Some(JsValue::from_str(&send_body(channel_id, text))));
            let req = Request::new_with_init(LINEWORKS_SEND_URL, &init)
                .map_err(|_| NotifyError("request"))?;
            let resp = self
                .fetcher
                .fetch_request(req)
                .await
                .map_err(|_| NotifyError("fetch"))?;
            if (200..300).contains(&resp.status().as_u16()) {
                Ok(())
            } else {
                Err(NotifyError("status"))
            }
        }))
    }
}

/// 送れない (binding か secret が無い)。警告は未通知のまま残り、次の回に送る
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn send<'a>(&'a self, _: Uuid, _: &'a str) -> BoxFuture<'a, Result<(), NotifyError>> {
        Box::pin(async { Err(NotifyError("not_configured")) })
    }
}
