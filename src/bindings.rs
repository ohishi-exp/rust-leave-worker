//! crates/leave の trait の Workers での実装: R2 の binding (`LEAVE_R2`・スキャンの inbox `LEAVE_INBOX`)・
//! 一番星 Worker の Service Binding (`ICHIBAN_DB`)・日本の祝日 (holidays-jp への外向きの `fetch`)・LINE WORKS への送信 (auth-worker の Service Binding `AUTH_WORKER`)・
//! チケットの検証 (auth-worker の RPC、Service Binding `AUTH_MCRED`)。
//!
//! 値と future は JS の値で `Send` でないので、`worker::send` の `SendWrapper`・`SendFuture` で包む (Workers は単一 thread)。
//! エラーは段の名前だけに落とす (**key・ランタイムの生のエラー文はログに出さない**)。
//!
//! このファイルは wasm 専用で、CI のテストと coverage の gate の外 (確かめるのは staging / 本番の実物)。

use futures_util::future::BoxFuture;
use leave::cron::{send_body, Notifier, NotifyError, NotifyTarget, LINEWORKS_SEND_URL};
use leave::holidays::{holidays_url, HolidaysApi, HolidaysError, HolidaysResponse};
use leave::ichiban::{IchibanApi, IchibanError, IchibanResponse};
use leave::inbox::{Inbox, InboxListed, InboxObject, TicketVerifier, VerifyError};
use leave::store::{PdfStore, StoreError};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use worker::send::{SendFuture, SendWrapper};
use worker::{Bucket, Fetch, Fetcher, Headers, Method, Request, RequestInit};

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

    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(SendFuture::new(async move {
            let put = self.0.put(key, bytes).execute().await;
            put.map(|_| ()).map_err(|_| StoreError("put"))
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

/// 日本の祝日 (holidays-jp。binding の無い外向きの `fetch`)
pub struct HolidaysJp;

/// 祝日の応答の本文の上限 (1 年ぶんは数 KB)
const HOLIDAYS_BODY_LIMIT: usize = 256 * 1024;

impl HolidaysApi for HolidaysJp {
    fn year(&self, year: i32) -> BoxFuture<'_, Result<HolidaysResponse, HolidaysError>> {
        Box::pin(SendFuture::new(async move {
            let mut init = RequestInit::new();
            init.with_method(Method::Get);
            let req =
                Request::new_with_init(&holidays_url(year), &init).map_err(|_| HolidaysError)?;
            let mut resp = Fetch::Request(req)
                .send()
                .await
                .map_err(|_| HolidaysError)?;
            let status = resp.status_code();
            let body = resp.bytes().await.map_err(|_| HolidaysError)?;
            if body.len() > HOLIDAYS_BODY_LIMIT {
                return Err(HolidaysError);
            }
            Ok(HolidaysResponse { status, body })
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
        target: NotifyTarget,
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
                .with_body(Some(JsValue::from_str(&send_body(target, text))));
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

/// スキャンの inbox (binding `LEAVE_INBOX`)。チケットは `customMetadata.ticket`
pub struct R2Inbox(SendWrapper<Bucket>);

impl R2Inbox {
    pub fn new(bucket: Bucket) -> Self {
        Self(SendWrapper::new(bucket))
    }
}

impl Inbox for R2Inbox {
    fn list(&self, limit: u32) -> BoxFuture<'_, Result<Vec<InboxListed>, StoreError>> {
        Box::pin(SendFuture::new(async move {
            let objects = self
                .0
                .list()
                .limit(limit)
                .execute()
                .await
                .map_err(|_| StoreError("list"))?;
            Ok(objects
                .objects()
                .into_iter()
                .map(|o| InboxListed {
                    key: o.key(),
                    size: o.size(),
                    uploaded: chrono::DateTime::from_timestamp_millis(
                        o.uploaded().as_millis() as i64
                    )
                    .unwrap_or_default(),
                })
                .collect())
        }))
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<InboxObject>, StoreError>> {
        Box::pin(SendFuture::new(async move {
            let object = self.0.get(key).execute().await;
            let Some(object) = object.map_err(|_| StoreError("get"))? else {
                return Ok(None);
            };
            let ticket = object
                .custom_metadata()
                .map_err(|_| StoreError("metadata"))?
                .remove("ticket");
            let body = object.body().ok_or(StoreError("get body"))?;
            let bytes = body.bytes().await.map_err(|_| StoreError("read body"))?;
            Ok(Some(InboxObject { bytes, ticket }))
        }))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(SendFuture::new(async move {
            self.0.delete(key).await.map_err(|_| StoreError("delete"))
        }))
    }
}

#[wasm_bindgen]
extern "C" {
    /// auth-worker の named entrypoint `MachineCredentialEntrypoint` (binding `AUTH_MCRED`)
    #[wasm_bindgen(extends = js_sys::Object)]
    type MachineCredentialRpc;

    #[wasm_bindgen(method, catch, js_name = verifyTicket)]
    fn verify_ticket(
        this: &MachineCredentialRpc,
        ticket: &str,
        purpose: &str,
    ) -> Result<js_sys::Promise, JsValue>;
}

/// チケットの検証 (auth-worker の RPC `verifyTicket`)。戻り値は JSON のまま crates/leave の `inbox::parse_ticket` に渡す。
/// 呼び出しの失敗 (throw・変換できない値) は一時的な失敗 (retry)。エラーの中身は捨てる
pub struct AuthTicketVerifier(SendWrapper<MachineCredentialRpc>);

impl AuthTicketVerifier {
    pub fn new(fetcher: Fetcher) -> Self {
        Self(SendWrapper::new(fetcher.into_rpc::<MachineCredentialRpc>()))
    }
}

impl TicketVerifier for AuthTicketVerifier {
    fn verify<'a>(
        &'a self,
        ticket: &'a str,
        purpose: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, VerifyError>> {
        Box::pin(SendFuture::new(async move {
            let promise = self
                .0
                .verify_ticket(ticket, purpose)
                .map_err(|_| VerifyError)?;
            let value = JsFuture::from(promise).await.map_err(|_| VerifyError)?;
            if value.is_null() || value.is_undefined() {
                return Ok(serde_json::Value::Null);
            }
            serde_wasm_bindgen::from_value(value).map_err(|_| VerifyError)
        }))
    }
}

/// 送れない (binding か secret が無い)。警告は未通知のまま残り、次の回に送る
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn available(&self) -> bool {
        false
    }

    fn send<'a>(&'a self, _: NotifyTarget, _: &'a str) -> BoxFuture<'a, Result<(), NotifyError>> {
        Box::pin(async { Err(NotifyError("not_configured")) })
    }
}
