use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct NhTag {
    #[serde(rename = "type")]
    pub tag_type: String,
    pub name: String,
}