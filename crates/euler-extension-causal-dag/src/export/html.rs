use super::graph::ViewerDag;
use super::palette::PALETTE_JSON;
use crate::input_error;
use euler_sdk::ExtensionError;

const RUNTIME: &str = include_str!("../../assets/viewer/runtime.js");
const REACT: &str = include_str!("../../assets/viewer/react.production.min.js");
const REACT_DOM: &str = include_str!("../../assets/viewer/react-dom.production.min.js");
const TOP_DOWN: &str = include_str!("../../assets/viewer/top-down.html");
const INDENTED_SPINE: &str = include_str!("../../assets/viewer/indented-spine.html");
const CONSTELLATION_3D: &str = include_str!("../../assets/viewer/constellation-3d.html");
const CONSTELLATION_3_5D: &str = include_str!("../../assets/viewer/constellation-3-5d.html");

const RUNTIME_MARKER: &str = "<!--__EULER_RUNTIME__-->";
const DAG_MARKER: &str = "/*__EULER_DAG__*/";
const PALETTE_MARKER: &str = "/*__EULER_PALETTE__*/";

pub(super) fn render_html(dag: &ViewerDag) -> Result<Vec<u8>, ExtensionError> {
    let dag_json = script_safe_json(
        &serde_json::to_string(dag)
            .map_err(|error| input_error(format!("causal-dag HTML encode failed: {error}")))?,
    );
    let palette_json = script_safe_json(PALETTE_JSON.trim());
    let pages = [
        ("view-top-down", TOP_DOWN),
        ("view-indented-spine", INDENTED_SPINE),
        ("view-constellation-3d", CONSTELLATION_3D),
        ("view-constellation-3-5d", CONSTELLATION_3_5D),
    ]
    .into_iter()
    .map(|(id, template)| render_page(template, &dag_json, &palette_json).map(|page| (id, page)))
    .collect::<Result<Vec<_>, _>>()?;

    let mut html =
        String::from("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n");
    html.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval'; style-src 'unsafe-inline'; font-src data:; img-src data:; connect-src 'none'; frame-src 'self' data: blob:;\">\n");
    html.push_str("<title>");
    html.push_str(&escape_html(&format!("Euler causal DAG · {}", dag.title)));
    html.push_str("</title>\n<style>html,body{margin:0;height:100%;background:#fdfcf9}iframe{position:fixed;inset:0;border:0;width:100%;height:100%}</style>\n</head>\n<body>\n");
    html.push_str(
        "<iframe id=\"frame\" title=\"Euler causal DAG viewer\" allow=\"fullscreen\"></iframe>\n",
    );
    for (id, page) in pages {
        html.push_str("<template id=\"");
        html.push_str(id);
        html.push_str("\">\n");
        html.push_str(&page);
        html.push_str("\n</template>\n");
    }
    html.push_str(PARENT_SCRIPT);
    html.push_str("\n</body>\n</html>\n");
    Ok(html.into_bytes())
}

fn render_page(
    template: &str,
    dag_json: &str,
    palette_json: &str,
) -> Result<String, ExtensionError> {
    for marker in [RUNTIME_MARKER, DAG_MARKER, PALETTE_MARKER] {
        if template.matches(marker).count() != 1 {
            return Err(input_error(format!(
                "causal-dag viewer template has an invalid `{marker}` injection point"
            )));
        }
    }
    let runtime = format!(
        "<script>\n{REACT}\n</script>\n<script>\n{REACT_DOM}\n</script>\n<script>\n{RUNTIME}\n</script>"
    );
    Ok(template
        .replace(RUNTIME_MARKER, &runtime)
        .replace(DAG_MARKER, dag_json)
        .replace(PALETTE_MARKER, palette_json))
}

fn script_safe_json(json: &str) -> String {
    json.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const PARENT_SCRIPT: &str = r#"<script>
const VIEWS = {
  "2D Top-Down.dc.html": "view-top-down",
  "2D Indented Spine.dc.html": "view-indented-spine",
  "Constellation 3D.dc.html": "view-constellation-3d",
  "Constellation 3.5D.dc.html": "view-constellation-3-5d"
};
function showView(name) {
  const template = document.getElementById(VIEWS[name]);
  if (!template) return;
  document.getElementById("frame").srcdoc = template.innerHTML;
  try { localStorage.setItem("euler.causal-dag.view", name); } catch (_) {}
}
window.addEventListener("message", (event) => {
  if (event.data && event.data.dagNav) showView(event.data.dagNav);
});
let initial = "2D Top-Down.dc.html";
try { initial = localStorage.getItem("euler.causal-dag.view") || initial; } catch (_) {}
if (!VIEWS[initial]) initial = "2D Top-Down.dc.html";
showView(initial);
</script>"#;

#[cfg(test)]
mod tests {
    use super::{render_html, script_safe_json};
    use crate::export::graph::ViewerDag;
    use serde_json::Value;

    #[test]
    fn script_json_escapes_html_breakouts() {
        assert_eq!(
            script_safe_json("{\"x\":\"</script>&\"}"),
            "{\"x\":\"\\u003c/script\\u003e\\u0026\"}"
        );
    }

    #[test]
    fn html_is_self_contained_and_injects_all_four_views() {
        let mut artifact: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/causal_dag/knuth_style_search/expected.causal-dag.json"
        ))
        .expect("fixture artifact");
        artifact["forest"]["nodes"][0]["title"] =
            Value::String("</script><script>window.pwned=true</script>".to_owned());
        let dag = ViewerDag::from_artifact(&artifact).expect("viewer DAG");
        let html = String::from_utf8(render_html(&dag).expect("HTML render")).expect("UTF-8");

        for id in [
            "view-top-down",
            "view-indented-spine",
            "view-constellation-3d",
            "view-constellation-3-5d",
        ] {
            assert!(html.contains(&format!("<template id=\"{id}\">")));
        }
        assert_eq!(html.matches("const __DAG =").count(), 4);
        assert!(!html.contains("__EULER_DAG__"));
        assert!(!html.contains("__EULER_PALETTE__"));
        assert!(!html.contains("__EULER_RUNTIME__"));
        assert!(!html.contains("fetch("));
        assert!(!html.contains("unpkg.com"));
        assert!(!html.contains("fonts.googleapis"));
        assert!(!html.contains("<script src="));
        assert!(!html.contains("</script><script>window.pwned"));
        assert!(html.contains("\\u003c/script\\u003e\\u003cscript\\u003ewindow.pwned"));
        assert!(html.contains("#7f97a8"));
        assert!(html.contains("connect-src 'none'"));
    }
}
