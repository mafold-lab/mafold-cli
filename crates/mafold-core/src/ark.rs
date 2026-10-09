//! The `ark-video` native driver — the device half of `mafold video …` when the
//! user brings their own BytePlus ModelArk key.
//!
//! Split of knowledge, same as `codex-responses`: **the credential lives here,
//! the product rules live server-side.** This driver does only what a vault
//! device can do — open the sealed payload, put the bearer key on the wire,
//! relay the vendor's answer verbatim. Landing a finished clip (围栏二: 成功先
//! 落地) is not its job: the agent hands the 24 h `video_url` to the server's
//! `ingestUrl`, which is the one door every byte enters the registry through.
//! Validation against the model table (known-bad combos, draft support) is
//! the server's `videoSubmit` job on the house-key path; here the vendor's own
//! refusal is quoted instead, so the two paths never disagree about a model.
//!
//! Three methods, all one HTTP round trip, no streaming:
//!   `video.submit`  {model, prompt, resolution?, ratio?, duration?, refs?,
//!                    generate_audio?, watermark?, draft?}  → the task as the
//!                    vendor returns it (`id` is the job id)
//!   `video.status`  {job_id} → the task; on `succeeded` it carries
//!                    `content.video_url` (24 h) and `usage.completion_tokens`
//!   `video.cancel`  {job_id} → the vendor's answer (only queued tasks cancel)

use serde_json::{json, Value};

use crate::connections::Runtime;
use crate::net;
use mafold_types::connections::ProviderInfo;

pub const DRIVER: &str = "ark-video";
const DEFAULT_BASE: &str = "https://ark.ap-southeast.bytepluses.com";
const TASKS: &str = "/api/v3/contents/generations/tasks";

/// What `tools/list` answers for a connection driven by this module — the
/// same shape MCP servers use, so `mafold connection methods <name>` and an
/// agent's tool discovery read it without a special case.
pub fn catalog() -> Value {
    json!({ "tools": [
        {
            "name": "video.submit",
            "title": "Submit a video generation",
            "description": "Start one Seedance job on ModelArk with this connection's key. Answers with the vendor task (`id` = job id). Use `video.status` to poll; on success hand `content.video_url` to `mafold attach <url>` so the clip lands in the registry (the vendor link expires in 24h).",
            "inputSchema": {
                "type": "object",
                "required": ["model", "prompt"],
                "properties": {
                    "model": { "type": "string", "description": "e.g. dreamina-seedance-2-5-260628" },
                    "prompt": { "type": "string" },
                    "resolution": { "type": "string", "enum": ["480p", "720p", "1080p"], "default": "720p" },
                    "ratio": { "type": "string", "description": "adaptive | 16:9 | 4:3 | 1:1 | 3:4 | 9:16 | 21:9 (forced adaptive when refs are given)", "default": "adaptive" },
                    "duration": { "type": "integer", "description": "seconds 4–30, or -1", "default": 5 },
                    "refs": { "type": "array", "items": { "type": "object", "required": ["role", "url"], "properties": {
                        "role": { "type": "string", "enum": ["first_frame", "last_frame", "reference_image", "reference_video", "reference_audio"] },
                        "url": { "type": "string" } } } },
                    "generate_audio": { "type": "boolean", "default": false },
                    "watermark": { "type": "boolean", "default": false },
                    "draft": { "type": "boolean", "description": "cheap draft render where the model supports it" }
                }
            },
            "readOnly": false
        },
        {
            "name": "video.status",
            "title": "Read a video job",
            "description": "The vendor task as-is: status queued|running|succeeded|failed|cancelled, `usage.completion_tokens` (the bill), and on success `content.video_url` (24h).",
            "inputSchema": { "type": "object", "required": ["job_id"], "properties": { "job_id": { "type": "string" } } },
            "readOnly": true
        },
        {
            "name": "video.cancel",
            "title": "Cancel a queued video job",
            "description": "Only a queued task can be cancelled; the vendor's refusal is quoted otherwise.",
            "inputSchema": { "type": "object", "required": ["job_id"], "properties": { "job_id": { "type": "string" } } },
            "readOnly": false
        }
    ]})
}

pub(crate) async fn run(
    rt: &Runtime,
    name: &str,
    conn: &Value,
    spec: &ProviderInfo,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let payload = rt.open_payload(conn)?;
    let key = payload
        .get(spec.auth.field.as_str())
        .and_then(Value::as_str)
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| format!("`{name}` holds no `{}` — re-add the connection", spec.auth.field))?
        .trim()
        .to_string();
    let base = payload
        .get("base_url")
        .and_then(Value::as_str)
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string());
    let headers = vec![
        (spec.auth.header.clone(), format!("{}{key}", spec.auth.prefix)),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    match method {
        "video.submit" => {
            let body = submit_body(params)?;
            let reply = net::http_request("POST", &format!("{base}{TASKS}"), &headers, Some(&body.to_string()))
                .await
                .map_err(|e| e.to_string())?;
            answer(reply)
        }
        "video.status" => {
            let job = job_id(params)?;
            let reply = net::http_request("GET", &format!("{base}{TASKS}/{job}"), &headers, None)
                .await
                .map_err(|e| e.to_string())?;
            answer(reply)
        }
        "video.cancel" => {
            let job = job_id(params)?;
            let reply = net::http_request("DELETE", &format!("{base}{TASKS}/{job}"), &headers, None)
                .await
                .map_err(|e| e.to_string())?;
            let mut v = answer(reply)?;
            if v.is_object() {
                v["job_id"] = json!(job);
                v["cancelled"] = json!(true);
            }
            Ok(v)
        }
        other => Err(format!(
            "{} offers `video.submit`, `video.status` and `video.cancel` — `{other}` is not one of them",
            spec.display
        )),
    }
}

/// The vendor's own words on failure — a billing system is quoted, never
/// paraphrased. `status >= 400` is an Err carrying `error.code` + `message`.
fn answer(reply: net::HttpReply) -> Result<Value, String> {
    let v: Value = serde_json::from_str(&reply.body).unwrap_or_else(|_| json!({ "raw": reply.body }));
    if reply.status >= 400 {
        let code = v["error"]["code"].as_str().or(v["code"].as_str()).unwrap_or("");
        let msg = v["error"]["message"].as_str().or(v["message"].as_str()).unwrap_or("");
        let text = format!("ark {}: {code} {msg}", reply.status).trim().to_string();
        return Err(if msg.is_empty() && code.is_empty() {
            format!("ark {}: {}", reply.status, v["raw"].as_str().unwrap_or("").chars().take(300).collect::<String>())
        } else {
            text
        });
    }
    Ok(v)
}

fn job_id(params: &Value) -> Result<String, String> {
    let job = params
        .get("job_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("job_id is required")?;
    if !job.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("job_id: unexpected characters".into());
    }
    Ok(job.to_string())
}

/// Build the vendor request. Shapes only; the model table's judgement (known
/// bad combos, draft support) lives on the server's house-key path, and the
/// vendor answers for itself here.
fn submit_body(params: &Value) -> Result<Value, String> {
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("model is required")?;
    let prompt = params
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("prompt is required")?;
    let mut content = vec![json!({ "type": "text", "text": prompt })];
    if let Some(refs) = params.get("refs").and_then(Value::as_array) {
        for r in refs {
            let role = r.get("role").and_then(Value::as_str).unwrap_or("");
            let url = r.get("url").and_then(Value::as_str).unwrap_or("");
            if !matches!(role, "first_frame" | "last_frame" | "reference_image" | "reference_video" | "reference_audio") {
                return Err(format!("refs: unknown role `{role}`"));
            }
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("refs: url must be http(s) — a chat attachment's public link".into());
            }
            let kind = match role {
                "reference_video" => "video_url",
                "reference_audio" => "audio_url",
                _ => "image_url",
            };
            content.push(json!({ "type": kind, kind: { "url": url }, "role": role }));
        }
    }
    let has_refs = content.len() > 1;
    let ratio = params.get("ratio").and_then(Value::as_str).unwrap_or("adaptive");
    if has_refs && ratio != "adaptive" {
        return Err("with a first frame or reference, ratio must be adaptive".into());
    }
    let mut body = json!({
        "model": model,
        "content": content,
        "resolution": params.get("resolution").and_then(Value::as_str).unwrap_or("720p"),
        "ratio": ratio,
        "duration": params.get("duration").and_then(Value::as_i64).unwrap_or(5),
        "generate_audio": params.get("generate_audio").and_then(Value::as_bool).unwrap_or(false),
        "watermark": params.get("watermark").and_then(Value::as_bool).unwrap_or(false),
    });
    if params.get("draft").and_then(Value::as_bool) == Some(true) {
        body["draft"] = json!(true);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_body_mirrors_the_vendor_shape() {
        let b = submit_body(&json!({
            "model": "dreamina-seedance-2-5-260628", "prompt": "a cup",
            "refs": [{ "role": "first_frame", "url": "https://cdn.mafold.com/x.jpg" }]
        }))
        .unwrap();
        assert_eq!(b["ratio"], "adaptive");
        assert_eq!(b["content"][1]["image_url"]["url"], "https://cdn.mafold.com/x.jpg");
        assert_eq!(b["content"][1]["role"], "first_frame");
        assert!(b.get("draft").is_none());
    }

    #[test]
    fn refs_force_adaptive_and_reject_odd_roles() {
        let e = submit_body(&json!({ "model": "m", "prompt": "p", "ratio": "9:16",
            "refs": [{ "role": "first_frame", "url": "https://a/b.jpg" }] })).unwrap_err();
        assert!(e.contains("adaptive"));
        let e = submit_body(&json!({ "model": "m", "prompt": "p",
            "refs": [{ "role": "hero", "url": "https://a/b.jpg" }] })).unwrap_err();
        assert!(e.contains("unknown role"));
    }

    #[test]
    fn vendor_errors_are_quoted_not_paraphrased() {
        let r = net::HttpReply { status: 400, headers: vec![], body: r#"{"error":{"code":"InvalidParameter","message":"draft not supported"}}"#.into() };
        assert_eq!(answer(r).unwrap_err(), "ark 400: InvalidParameter draft not supported");
        let ok = net::HttpReply { status: 200, headers: vec![], body: r#"{"id":"cgt-1","status":"queued"}"#.into() };
        assert_eq!(answer(ok).unwrap()["id"], "cgt-1");
    }

    #[test]
    fn catalog_lists_three_tools() {
        let c = catalog();
        let names: Vec<&str> = c["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["video.submit", "video.status", "video.cancel"]);
    }
}
