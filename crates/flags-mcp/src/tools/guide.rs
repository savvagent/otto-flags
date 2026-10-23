//! Per-language guides for adding an SDK to a project. Read-only, free, and
//! static apart from the app's own name, id, environments and public client
//! key; the content lives in [`crate::guide`].

use flags_core::apps::AppsExt;
use flags_core::scopes::FLAGS_READ;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::guide::{self, GuideError};
use crate::server::{Flags, McpResult};

/// How much of an unknown `language` an error echoes back.
const ECHO_CHARS: usize = 64;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SdkGuideArgs {
    /// The language or framework to integrate: python, typescript, node, react,
    /// vue, svelte, sveltekit, solid, angular, astro, nextjs, remix, go, java,
    /// rust, ios, android. Common aliases (js, py, golang, swift, kotlin) work.
    pub language: String,
    /// Optional app name (or id). Fills the guide with that app's id, environments
    /// and public client key. Omitted: the organization's only app is used, or, if
    /// it has several, they are listed in `chooseFrom` for you to pick one.
    #[serde(default)]
    pub app: Option<String>,
}

/// The most apps `chooseFrom` lists.
const MAX_CHOICES: usize = 50;

/// An unknown language is the caller's argument error. The message echoes at
/// most [`ECHO_CHARS`] characters of what was given.
fn guide_error(e: GuideError) -> ErrorData {
    let GuideError::Unknown { given, supported } = e;
    let given: String = given.chars().take(ECHO_CHARS).collect();
    crate::error::from_core(&flags_core::Error::Invalid(format!(
        "unknown language {given:?}; supported: {}",
        supported.join(", ")
    )))
}

#[tool_router(router = guide_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "sdk_guide",
        description = "How to add an Otto Flags SDK to a project in one language or framework: \
                       an ordered integrate-end-to-end recipe (including who stores the server \
                       key and how to verify), then install (pinned to a commit), initialize, \
                       evaluate a flag, and framework snippets. Pass `app` to fill in that app's \
                       id, environments and public client key; without it the organization's \
                       only app is used, or its apps are listed in `chooseFrom`. Returns \
                       Markdown. Free.",
        annotations(read_only_hint = true)
    )]
    pub async fn sdk_guide(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SdkGuideArgs>,
    ) -> Result<Json<out::SdkGuideOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        // Validate before opening a transaction: a typo records nothing.
        let language = guide::lookup(&args.language).map_err(guide_error)?.name;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "sdk_guide").await?;
        let mut defaulted = false;
        let mut choose_from = Vec::new();
        let app = match &args.app {
            Some(name) => Some(tx.resolve_app(name).await.mcp()?),
            None => {
                let mut apps = tx.list_apps().await.mcp()?;
                match apps.len() {
                    0 => None,
                    1 => {
                        defaulted = true;
                        apps.pop()
                    }
                    _ => {
                        choose_from = apps
                            .into_iter()
                            .filter(|a| {
                                flags_core::eval::validate_name("app name", &a.name).is_ok()
                            })
                            .take(MAX_CHOICES)
                            .map(|a| out::GuideAppRef {
                                name: a.name,
                                id: a.id,
                            })
                            .collect();
                        None
                    }
                }
            }
        };
        tx.commit().await.mcp()?;
        let (language, text) = guide::render(language, app.as_ref()).map_err(guide_error)?;
        Ok(Json(out::SdkGuideOut {
            language: language.to_string(),
            guide: text,
            app: app.map(|a| out::GuideApp {
                name: a.name,
                id: a.id,
                client_key: a.client_key,
            }),
            app_defaulted: defaulted,
            choose_from,
            supported: guide::supported().into_iter().map(String::from).collect(),
        }))
    }
}
