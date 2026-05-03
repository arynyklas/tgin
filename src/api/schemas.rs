use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct AddWebhookRouteSch {
    #[serde(default)]
    pub url: String,
    /// Per-request timeout (milliseconds) for outbound POSTs to this
    /// downstream. Omit to use `WebhookRoute::DEFAULT_REQUEST_TIMEOUT`.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
}

#[derive(Deserialize, Debug)]
pub struct AddLongpullRouteSch {
    #[serde(default)]
    pub path: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum AddRouteTypeSch {
    Webhook(AddWebhookRouteSch),
    Longpull(AddLongpullRouteSch),
}

/// Body of `POST /<base_path>/route`.
///
/// Historically this struct also carried a `sublevel: i8` field intended
/// for hierarchical insertion (e.g. "attach this child to the LB N levels
/// down"). The runtime never honored it — the value was discarded with
/// `let _ = sublevel;` in `Tgin::run_async` and the route always landed at
/// the top. A field that silently no-ops is worse than no field at all:
/// operators read the docs, set it, and were quietly lied to. The field
/// has been removed; if hierarchical insertion is ever implemented it must
/// address LBs by a stable id exposed through `/routes`, not by an
/// integer level that has no defined semantics in a heterogeneous tree.
#[derive(Deserialize, Debug)]
pub struct AddRouteSch {
    #[serde(flatten)]
    pub typee: AddRouteTypeSch,
}

#[derive(Deserialize, Debug)]
pub struct RmWebhookRouteSch {
    #[serde(default)]
    pub url: String,
}

#[derive(Deserialize, Debug)]
pub struct RmLongpullRouteSch {
    #[serde(default)]
    pub path: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum RmRouteTypeSch {
    Webhook(RmWebhookRouteSch),
    Longpull(RmLongpullRouteSch),
}

#[derive(Deserialize, Debug)]
pub struct RmRouteSch {
    #[serde(flatten)]
    pub typee: RmRouteTypeSch,
}
