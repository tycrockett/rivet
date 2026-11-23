//! The web tab tools of Bolt's MCP server (`useMcpBridge.ts`'s `browse` and
//! `getInspectedElement`) over Rivet's web tabs (`web.rs`). `browse` acts on
//! the web tab showing in the current workspace, and navigates, clicks and
//! types only while that tab is armed (its toolbar's AI switch). What needs
//! a picture of the page or Chromium's debugger (`screenshot`, `draw`,
//! `clear`, `network`, `devtools`) says it is not supported here.

use serde_json::Value;

use nsh_core::{WebInput, prelude::*};

use super::ToolResult;
use crate::{state::Shell, web};

/// What a read or a log answers at most (Bolt's `MAX_READ`).
const MAX_READ: usize = 20_000;

/// Prepended to the listed descriptions of the tools Rivet backs only in
/// part.
pub const NOTES: &[(&str, &str)] = &[
    ("browse", "In Rivet, `screenshot`, `draw`, `clear`, `network` and `devtools` return an error."),
    ("getInspectedElement", "In Rivet it has no screenshot of the element."),
];

pub async fn call(shell: Shell, name: &str, args: &Value) -> Option<ToolResult> {
    Some(match name {
        "getInspectedElement" => inspected(),
        "browse" => browse(shell, args).await,
        _ => return None,
    })
}

/// `getInspectedElement` (`handleGetInspectedElement`): the element last
/// shift-clicked in a web tab, without Bolt's cropped screenshot.
fn inspected() -> ToolResult {
    let el = web::inspected().ok_or("Nothing inspected yet. Ask the user to hold shift and click the element in a Rivet web tab.")?;
    Ok(serde_json::to_string_pretty(&el).unwrap_or_default())
}

fn first_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(at, _)| &s[..at])
}

fn last_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    s.char_indices().nth(count - n).map_or(s, |(at, _)| &s[at..])
}

/// `browse` (`handleBrowse`).
async fn browse(shell: Shell, args: &Value) -> ToolResult {
    let id = web::shown(shell).ok_or("No web tab is showing in Rivet. Ask the user to open one with `browser <url>`.")?;
    let web = shell.web(id).ok_or("The web tab is gone")?;
    let arg = |key: &str| args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let action = arg("action").unwrap_or_default();
    let selector = arg("selector");
    let sel = serde_json::to_string(selector.unwrap_or_default()).unwrap_or_default();

    if matches!(action, "navigate" | "click" | "type") {
        if !web.armed {
            return Err(format!(
                "This web tab is not armed, so {action} is refused. Ask the user to click the AI switch in the web tab's toolbar, then try again."
            ));
        }
        if action != "navigate" && !web_input_available().await {
            return Err(format!("{action} needs trusted input, which this Rivet build is not granted (`webview.input` in nsh.json)."));
        }
    }

    match action {
        "navigate" => {
            let url = arg("url").ok_or("navigate needs a url.")?;
            web::navigate(shell, id, url);
            Ok(format!("Navigating to {url}"))
        }
        "click" => {
            let selector = selector.ok_or("click needs a selector.")?;
            let at = web::evaluate(
                id,
                format!(
                    "(()=>{{const n=document.querySelector({sel});if(!n)return null;\
                     n.scrollIntoView({{block:\"center\",inline:\"center\"}});const r=n.getBoundingClientRect();\
                     return {{x:r.left+r.width/2,y:r.top+r.height/2}}}})()"
                ),
            )
            .await?;
            let (Some(x), Some(y)) = (at["x"].as_f64(), at["y"].as_f64()) else {
                return Err(format!("No element matches {selector}"));
            };
            web::send_input(id, WebInput::Click { x: Points(x as f32), y: Points(y as f32) });
            Ok(format!("Clicked {selector}"))
        }
        "type" => {
            let selector = selector.ok_or("type needs a selector.")?;
            let text = arg("text");
            // Selecting what is there first makes the text replace it.
            let delete = if text.is_some() { "" } else { "document.execCommand(\"delete\");" };
            let hit = web::evaluate(
                id,
                format!(
                    "(()=>{{const n=document.querySelector({sel});if(!n)return false;n.focus();\
                     if(typeof n.select===\"function\")n.select();else document.execCommand(\"selectAll\");\
                     {delete}return true}})()"
                ),
            )
            .await?;
            if hit != Value::Bool(true) {
                return Err(format!("No element matches {selector}"));
            }
            if let Some(text) = text {
                web::send_input(id, WebInput::Text(text.to_owned()));
            }
            Ok(format!("Typed into {selector}"))
        }
        "read" => {
            let script = match selector {
                Some(_) => format!("(()=>{{const n=document.querySelector({sel});return n?n.innerText:null}})()"),
                None => "document.body.innerText".to_owned(),
            };
            let read = web::evaluate(id, script).await?;
            let Some(read) = read.as_str() else {
                return Err(format!("No element matches {}", selector.unwrap_or_default()));
            };
            Ok(format!("{}\n\n{}", web.url, first_chars(read, MAX_READ)))
        }
        "console" => {
            let lines = web::console(id);
            if lines.is_empty() {
                return Ok(format!("{}\n\nNo console entries since this page loaded.", web.url));
            }
            Ok(format!("{}\n\n{}", web.url, last_chars(&lines.join("\n"), MAX_READ)))
        }
        "screenshot" | "draw" | "clear" | "network" | "devtools" => Err(format!("browse {action} is not supported in Rivet")),
        _ => Err(format!("Unknown browse action: {action}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_cuts_stay_on_boundaries() {
        assert_eq!(first_chars("héllo", 2), "hé");
        assert_eq!(last_chars("héllo", 4), "éllo");
        assert_eq!(last_chars("ab", 5), "ab");
    }
}
