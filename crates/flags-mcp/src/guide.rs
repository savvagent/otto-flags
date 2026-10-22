//! Per-language guides for consuming the otto-flags SDKs, rendered for the `sdk_guide` tool.
//!
//! Content is Markdown under `content/guides/`, compiled in. Static: nothing here reads a
//! database or the network. Install routes are commit-pinned source routes (`<commit>`); no
//! guide may mention a registry or publication, which `tests::no_guide_mentions_a_registry`
//! enforces.
//!
//! A guide may carry three placeholders, filled from the app the caller names or, without one,
//! left as angle-bracket placeholders a human fills in:
//!
//! | Placeholder      | From an app          | Without one    |
//! |------------------|----------------------|----------------|
//! | `{{APP_NAME}}`   | `FlagApp.name`       | `<app-name>`   |
//! | `{{APP_ID}}`     | `FlagApp.id`         | `<app-id>`     |
//! | `{{CLIENT_KEY}}` | `FlagApp.client_key` | `<client-key>` |
//!
//! Only the public client key is ever substituted. A server key is shown once by `create_app`
//! or `rotate_app_keys` and is never stored in clear, so no guide can contain one; guides name
//! it in words and read it from the environment. Rendering is a plain string replace: app names
//! are restricted to letters, digits and `-_.`, so a substituted value cannot add Markdown
//! structure.

use flags_core::apps::FlagApp;

/// One SDK language: its canonical name, the other names an agent may use for it, and its guide.
#[derive(Debug)]
pub struct Lang {
    /// Canonical name, also the file stem under `content/guides/`.
    pub name: &'static str,
    /// Other names accepted for this language, lower-case.
    pub aliases: &'static [&'static str],
    /// The guide, with placeholders unfilled.
    pub md: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuideError {
    #[error("no SDK guide for {given:?}; supported: {}", supported.join(", "))]
    Unknown {
        given: String,
        supported: Vec<&'static str>,
    },
}

pub const LANGUAGES: &[Lang] = &[
    Lang {
        name: "python",
        aliases: &["py", "python3"],
        md: include_str!("../content/guides/python.md"),
    },
    Lang {
        name: "typescript",
        aliases: &["ts", "js", "javascript"],
        md: include_str!("../content/guides/typescript.md"),
    },
    Lang {
        name: "node",
        aliases: &["nodejs", "node-server"],
        md: include_str!("../content/guides/node.md"),
    },
    Lang {
        name: "react",
        aliases: &[],
        md: include_str!("../content/guides/react.md"),
    },
    Lang {
        name: "vue",
        aliases: &[],
        md: include_str!("../content/guides/vue.md"),
    },
    Lang {
        name: "svelte",
        aliases: &[],
        md: include_str!("../content/guides/svelte.md"),
    },
    Lang {
        name: "sveltekit",
        aliases: &[],
        md: include_str!("../content/guides/sveltekit.md"),
    },
    Lang {
        name: "solid",
        aliases: &[],
        md: include_str!("../content/guides/solid.md"),
    },
    Lang {
        name: "angular",
        aliases: &[],
        md: include_str!("../content/guides/angular.md"),
    },
    Lang {
        name: "astro",
        aliases: &[],
        md: include_str!("../content/guides/astro.md"),
    },
    Lang {
        name: "nextjs",
        aliases: &["next"],
        md: include_str!("../content/guides/nextjs.md"),
    },
    Lang {
        name: "remix",
        aliases: &[],
        md: include_str!("../content/guides/remix.md"),
    },
];

/// The canonical name of every language with a guide, in [`LANGUAGES`] order.
pub fn supported() -> Vec<&'static str> {
    LANGUAGES.iter().map(|l| l.name).collect()
}

/// The language `language` names, matched case-insensitively against names and aliases.
pub fn lookup(language: &str) -> Result<&'static Lang, GuideError> {
    let wanted = language.trim().to_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.name == wanted || l.aliases.contains(&wanted.as_str()))
        .ok_or_else(|| GuideError::Unknown {
            given: language.to_string(),
            supported: supported(),
        })
}

/// Renders the guide for `language`, filled from `app` when given.
///
/// Returns the canonical language name with the text, so a caller that passed an alias can
/// report which guide it got.
///
/// ```
/// let (name, text) = flags_mcp::guide::render("py", None).unwrap();
/// assert_eq!(name, "python");
/// assert!(text.contains("<app-id>"));
/// assert!(flags_mcp::guide::render("cobol", None).is_err());
/// ```
pub fn render(language: &str, app: Option<&FlagApp>) -> Result<(&'static str, String), GuideError> {
    let lang = lookup(language)?;
    let (name, id, key) = match app {
        Some(a) => (a.name.clone(), a.id.to_string(), a.client_key.clone()),
        None => (
            "<app-name>".to_string(),
            "<app-id>".to_string(),
            "<client-key>".to_string(),
        ),
    };
    let text = lang
        .md
        .replace("{{APP_NAME}}", &name)
        .replace("{{APP_ID}}", &id)
        .replace("{{CLIENT_KEY}}", &key);
    Ok((lang.name, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
    }

    fn guide(name: &str) -> &'static str {
        LANGUAGES
            .iter()
            .find(|l| l.name == name)
            .unwrap_or_else(|| panic!("no language {name:?}"))
            .md
    }

    fn sample_app() -> FlagApp {
        // `FlagApp` derives Serialize but not Deserialize: build it directly.
        FlagApp {
            id: "11111111-2222-3333-4444-555555555555".parse().unwrap(),
            org_id: otto_tenant::ids::OrgId::from(
                "66666666-7777-8888-9999-000000000000"
                    .parse::<uuid::Uuid>()
                    .unwrap(),
            ),
            name: "checkout-web".into(),
            environments: vec!["production".into()],
            client_key: "sdk_abc123".into(),
            created_at: "2026-01-01T00:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn language_lookup_is_case_insensitive_and_takes_aliases() {
        assert_eq!(render("PYTHON", None).unwrap().0, "python");
        assert_eq!(render("py", None).unwrap().0, "python");
        assert_eq!(render("Python3", None).unwrap().0, "python");
        for l in LANGUAGES {
            assert_eq!(lookup(l.name).unwrap().name, l.name);
            for a in l.aliases {
                assert_eq!(lookup(a).unwrap().name, l.name, "alias {a:?}");
            }
        }
    }

    #[test]
    fn names_and_aliases_are_lower_case_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for l in LANGUAGES {
            for n in std::iter::once(&l.name).chain(l.aliases) {
                assert_eq!(*n, n.to_lowercase(), "{n:?} is not lower-case");
                assert!(seen.insert(*n), "{n:?} names two languages");
            }
        }
        assert_eq!(supported().len(), LANGUAGES.len());
    }

    #[test]
    fn unknown_language_lists_what_is_supported() {
        let Err(GuideError::Unknown { given, supported }) = render("cobol", None) else {
            panic!("cobol resolved")
        };
        assert_eq!(given, "cobol");
        assert_eq!(supported, super::supported());
        assert!(supported.contains(&"python"));
        assert!(matches!(render("", None), Err(GuideError::Unknown { .. })));
        let msg = lookup("cobol").unwrap_err().to_string();
        assert!(msg.contains("cobol") && msg.contains("python"), "{msg}");
    }

    #[test]
    fn placeholders_are_filled_from_the_app_or_left_as_angle_brackets() {
        let app = sample_app();
        let mut used = 0;
        for l in LANGUAGES {
            let anon = render(l.name, None).unwrap().1;
            let with = render(l.name, Some(&app)).unwrap().1;
            assert!(!anon.contains("{{"), "{}: unreplaced placeholder", l.name);
            assert!(!with.contains("{{"), "{}: unreplaced placeholder", l.name);
            for (ph, anon_value, app_value) in [
                ("{{APP_NAME}}", "<app-name>", "checkout-web"),
                (
                    "{{APP_ID}}",
                    "<app-id>",
                    "11111111-2222-3333-4444-555555555555",
                ),
                ("{{CLIENT_KEY}}", "<client-key>", "sdk_abc123"),
            ] {
                if l.md.contains(ph) {
                    used += 1;
                    assert!(
                        anon.contains(anon_value),
                        "{}: {ph} -> {anon_value}",
                        l.name
                    );
                    assert!(with.contains(app_value), "{}: {ph} -> {app_value}", l.name);
                    assert!(!with.contains(anon_value), "{}: {ph} left unfilled", l.name);
                }
            }
        }
        // Otherwise this test proves nothing about substitution.
        assert!(used > 0, "no guide uses a placeholder");
    }

    #[test]
    fn no_rendering_contains_a_server_key_prefix() {
        let app = sample_app();
        for l in LANGUAGES {
            for text in [
                render(l.name, None).unwrap().1,
                render(l.name, Some(&app)).unwrap().1,
            ] {
                assert!(!text.contains("srv_"), "{}", l.name);
            }
        }
    }

    #[test]
    fn every_guide_has_the_commit_pin_and_the_standard_sections() {
        for l in LANGUAGES {
            assert!(l.md.contains("<commit>"), "{}: no commit pin", l.name);
            for h in ["## Install", "## Initialize", "## Evaluate a flag"] {
                assert!(
                    l.md.lines().any(|line| line.trim_end() == h),
                    "{}: missing {h}",
                    l.name
                );
            }
        }
    }

    /// Words that would claim or imply a package is available from a registry.
    // Not "publish": Gradle's `publishToMavenLocal` task name is a legitimate local-install command.
    pub(crate) const FORBIDDEN: &[&str] = &[
        "pypi",
        "npmjs",
        "crates.io",
        "maven central",
        "mavencentral",
        "jitpack",
        "published",
        "publishing",
        "publication",
        "registry",
    ];

    #[test]
    fn no_guide_mentions_a_registry() {
        for l in LANGUAGES {
            let lower = l.md.to_lowercase();
            for w in FORBIDDEN {
                assert!(!lower.contains(w), "{}: mentions {w:?}", l.name);
            }
        }
    }

    /// (language, manifest path from the repo root, identifier the manifest must declare,
    /// identifier the guide must use). Keeps the guides honest about names.
    const MANIFESTS: &[(&str, &str, &str, &str)] = &[
        (
        "python",
        "packages/python-server/pyproject.toml",
        "name = \"otto-flags\"",
        "otto-flags @ git+https://github.com/savvagent/otto-flags@<commit>#subdirectory=packages/python-server",
    ),
    (
        "typescript",
        "packages/typescript/package.json",
        "\"name\": \"@otto-flags/sdk\"",
        "@otto-flags/sdk",
    ),
    (
        "node",
        "packages/node-server/package.json",
        "\"name\": \"@otto-flags/node-server\"",
        "@otto-flags/node-server",
    ),
    (
        "react",
        "packages/react/package.json",
        "\"name\": \"@otto-flags/react\"",
        "@otto-flags/react",
    ),
    (
        "vue",
        "packages/vue/package.json",
        "\"name\": \"@otto-flags/vue\"",
        "@otto-flags/vue",
    ),
    (
        "svelte",
        "packages/svelte/package.json",
        "\"name\": \"@otto-flags/svelte\"",
        "@otto-flags/svelte",
    ),
    (
        "sveltekit",
        "packages/sveltekit/package.json",
        "\"name\": \"@otto-flags/sveltekit\"",
        "@otto-flags/sveltekit",
    ),
    (
        "solid",
        "packages/solid/package.json",
        "\"name\": \"@otto-flags/solid\"",
        "@otto-flags/solid",
    ),
    (
        "angular",
        "packages/angular/package.json",
        "\"name\": \"@otto-flags/angular\"",
        "@otto-flags/angular",
    ),
    (
        "astro",
        "packages/astro/package.json",
        "\"name\": \"@otto-flags/astro\"",
        "@otto-flags/astro",
    ),
    (
        "nextjs",
        "packages/nextjs/package.json",
        "\"name\": \"@otto-flags/nextjs\"",
        "@otto-flags/nextjs",
    ),
    (
        "remix",
        "packages/remix/package.json",
        "\"name\": \"@otto-flags/remix\"",
        "@otto-flags/remix",
    ),
    ];

    #[test]
    fn guide_names_match_the_package_manifests() {
        for (lang, manifest, in_manifest, in_guide) in MANIFESTS {
            let m = std::fs::read_to_string(repo(manifest))
                .unwrap_or_else(|e| panic!("{manifest}: {e}"));
            assert!(
                m.contains(in_manifest),
                "{manifest} no longer declares {in_manifest}"
            );
            assert!(
                guide(lang).contains(in_guide),
                "{lang} guide must contain {in_guide}"
            );
        }
        for l in LANGUAGES {
            assert!(
                MANIFESTS.iter().any(|m| m.0 == l.name),
                "{}: no manifest entry",
                l.name
            );
        }
    }

    /// Every non-test source file under `dir`, concatenated.
    fn source_text(dir: &std::path::Path) -> String {
        let mut text = String::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
                let path = entry.unwrap().path();
                let file = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    stack.push(path);
                } else if !file.contains(".test.") && !file.contains(".spec.") {
                    text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
                }
            }
        }
        text
    }

    /// True when `name` occurs in `text` as a whole identifier.
    fn mentions_identifier(text: &str, name: &str) -> bool {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        text.match_indices(name).any(|(i, _)| {
            !text[..i].chars().next_back().is_some_and(is_ident)
                && !text[i + name.len()..].chars().next().is_some_and(is_ident)
        })
    }

    #[test]
    fn javascript_import_names_exist_in_the_packages() {
        let mut checked = 0;
        for l in LANGUAGES {
            for line in l.md.lines().map(str::trim) {
                let Some(rest) = line.strip_prefix("import {") else {
                    continue;
                };
                let Some((names, from)) = rest.split_once("} from ") else {
                    continue;
                };
                let spec = from.trim().trim_end_matches(';').trim_matches(['"', '\'']);
                let Some(pkg) = spec.strip_prefix("@otto-flags/") else {
                    continue;
                };
                let pkg = pkg.split('/').next().unwrap();
                let (_, manifest, _, _) = MANIFESTS
                    .iter()
                    .find(|m| m.3 == format!("@otto-flags/{pkg}"))
                    .unwrap_or_else(|| panic!("{}: imports unknown package {spec}", l.name));
                let src = repo(manifest).parent().unwrap().join("src");
                let text = source_text(&src);
                for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    // `import { a as b }` and `import { type T }` name the exported identifier.
                    let name = name
                        .trim_start_matches("type ")
                        .split(" as ")
                        .next()
                        .unwrap();
                    checked += 1;
                    assert!(
                        mentions_identifier(&text, name),
                        "{} guide imports {name} from {spec}, which {pkg}'s src does not mention",
                        l.name
                    );
                }
            }
        }
        assert!(checked > 0, "no javascript guide imports anything");
    }

    #[test]
    fn python_import_name_exists_in_the_package() {
        let init =
            std::fs::read_to_string(repo("packages/python-server/src/otto_flags/__init__.py"))
                .expect("the otto_flags package");
        let g = guide("python");
        assert!(g.contains("from otto_flags import"));
        // Every name the guide imports from the package is one the package exports.
        let mut imported = 0;
        for line in g.lines() {
            let Some(names) = line.trim().strip_prefix("from otto_flags import ") else {
                continue;
            };
            for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                imported += 1;
                assert!(
                    init.contains(&format!("\"{name}\"")),
                    "python guide imports {name}, which otto_flags does not export"
                );
            }
        }
        assert!(imported > 0, "python guide imports nothing from otto_flags");
    }
}
