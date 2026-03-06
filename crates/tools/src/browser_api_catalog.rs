use std::sync::Arc;

use {
    async_trait::async_trait,
    moltis_agents::tool_registry::{AgentTool, ToolEffectClass},
    moltis_browser::BrowserManager,
    serde_json::{Value, json},
};

use crate::error::Error;

pub struct BrowserApiCatalogTool {
    manager: Arc<BrowserManager>,
}

impl BrowserApiCatalogTool {
    pub fn new(manager: Arc<BrowserManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl AgentTool for BrowserApiCatalogTool {
    fn name(&self) -> &str {
        "browser_api_catalog"
    }

    fn categories(&self) -> &'static [&'static str] {
        &["web"]
    }

    fn description(&self) -> &str {
        "Read or export a previously captured browser API catalog by handle. \
         Use mode='summary' to get a bounded shape-only summary, or mode='export' to write the full redacted catalog to disk."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["catalog_handle", "mode"],
            "properties": {
                "catalog_handle": {
                    "type": "string",
                    "description": "Catalog handle returned by browser.stop_api_capture."
                },
                "mode": {
                    "type": "string",
                    "enum": ["summary", "export"],
                    "description": "summary returns a bounded shape-only view; export writes the full redacted catalog to disk."
                }
            }
        })
    }

    fn side_effect_class(&self) -> ToolEffectClass {
        ToolEffectClass::LocalMutation
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let handle = params
            .get("catalog_handle")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'catalog_handle' parameter"))?;
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'mode' parameter"))?;

        match mode {
            "summary" => self
                .manager
                .get_api_catalog_summary(handle)
                .await
                .map_err(anyhow::Error::from),
            "export" => {
                let (path, bytes) = self.manager.export_api_catalog(handle).await?;
                Ok(json!({
                    "catalog_handle": handle,
                    "path": path,
                    "format": "json",
                    "bytes": bytes,
                }))
            },
            _ => Err(Error::message(format!("unsupported mode: {mode}")).into()),
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_handle_and_mode() {
        let tool = BrowserApiCatalogTool::new(Arc::new(BrowserManager::default()));
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|value| value == "catalog_handle"));
        assert!(required.iter().any(|value| value == "mode"));
    }

    #[tokio::test]
    async fn summary_requires_known_handle() {
        let tool = BrowserApiCatalogTool::new(Arc::new(BrowserManager::default()));
        let error = tool
            .execute(json!({
                "catalog_handle": "missing",
                "mode": "summary"
            }))
            .await
            .expect_err("missing handle should fail");
        assert!(error.to_string().contains("unknown api catalog handle"));
    }
}
