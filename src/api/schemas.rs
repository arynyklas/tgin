use serde::Deserialize;

fn default_sublevel() -> i8 {
    0
}

#[derive(Deserialize, Debug)]
pub struct AddWebhookRoute {
    #[serde(default)]
    pub url: String,
}

#[derive(Deserialize, Debug)]
pub struct AddLongpullRoute {
    #[serde(default)]
    pub path: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum RouteType {
    Webhook(AddWebhookRoute),
    Longpull(AddLongpullRoute),
}

#[derive(Deserialize, Debug)]
pub struct AddRoute {
    #[serde(flatten)]
    pub typee: RouteType,
    #[serde(default = "default_sublevel")]
    pub sublevel: i8,
}
