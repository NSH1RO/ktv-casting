//! Bounded, read-only SOAP probes. Preserve device replies instead of coercing
//! unsupported/malformed progress and volume fields to zero.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use url::Url;

const READ_ACTIONS: &[&str] = &[
    "GetProtocolInfo",
    "GetCurrentConnectionIDs",
    "GetCurrentConnectionInfo",
    "GetDeviceCapabilities",
    "GetMediaInfo",
    "GetTransportSettings",
    "GetCurrentTransportActions",
    "GetTransportInfo",
    "GetPositionInfo",
    "GetVolume",
    "GetMute",
    "GetLoudness",
    "GetVolumeDB",
    "GetVolumeDBRange",
    "ListPresets",
];

struct Service {
    kind: String,
    urn: String,
    control: Url,
    scpd: Url,
    actions: Vec<String>,
    channels: Vec<String>,
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn services(description: &str, location: &Url) -> Result<Vec<Service>> {
    let doc = roxmltree::Document::parse(description)?;
    let base = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("URLBase"))
        .and_then(|n| n.text())
        .map(|s| location.join(s.trim()))
        .transpose()?
        .unwrap_or_else(|| location.clone());
    doc.descendants()
        .filter(|n| n.has_tag_name("service"))
        .map(|service| {
            let field = |name| {
                service
                    .children()
                    .find(|n| n.has_tag_name(name))
                    .and_then(|n| n.text())
                    .map(str::trim)
                    .context("Incomplete service description")
            };
            let urn = field("serviceType")?.to_owned();
            Ok(Service {
                kind: urn.split(':').nth(3).unwrap_or("unknown").to_owned(),
                urn,
                control: base.join(field("controlURL")?)?,
                scpd: base.join(field("SCPDURL")?)?,
                actions: Vec::new(),
                channels: Vec::new(),
            })
        })
        .collect()
}

fn parse_reply(body: &str, action: &str) -> Result<(BTreeMap<String, String>, Option<Value>)> {
    let doc = roxmltree::Document::parse(body)?;
    let soap_body = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("Body"))
        .context("Missing SOAP Body")?;
    if let Some(fault) = soap_body.children().find(|n| n.has_tag_name("Fault")) {
        let field = |tag| {
            fault
                .descendants()
                .find(|n| n.has_tag_name(tag))
                .and_then(|n| n.text())
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        return Ok((
            BTreeMap::new(),
            Some(json!({
                "errorCode": field("errorCode"), "errorDescription": field("errorDescription"),
                "faultstring": field("faultstring"),
            })),
        ));
    }
    let expected = format!("{action}Response");
    let response = soap_body
        .children()
        .find(|n| n.has_tag_name(expected.as_str()))
        .context("Missing expected SOAP action response")?;
    Ok((
        response
            .children()
            .filter(|n| n.is_element())
            .map(|n| {
                (
                    n.tag_name().name().to_owned(),
                    n.text().unwrap_or("").trim().to_owned(),
                )
            })
            .collect(),
        None,
    ))
}

struct Recorder {
    client: reqwest::Client,
    root: PathBuf,
    records: Vec<Value>,
}

impl Recorder {
    async fn query(
        &mut self,
        service: &Service,
        action: &str,
        args: &[(&str, String)],
    ) -> Result<Value> {
        // An explicit allowlist, rather than a Get prefix, prevents accidental controls.
        if !READ_ACTIONS.contains(&action) {
            bail!("Non-read-only action rejected: {action}");
        }
        let mut record = json!({"service": service.kind, "service_type": service.urn,
            "action": action, "url": service.control.as_str(), "args": args,
            "time_utc": chrono::Utc::now().to_rfc3339()});
        if !service.actions.iter().any(|a| a == action) {
            record["result"] = json!("not_advertised");
            self.records.push(record.clone());
            return Ok(record);
        }
        let args_xml: String = args
            .iter()
            .map(|(k, v)| format!("<{k}>{}</{k}>", escape(v)))
            .collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{}">{args_xml}</u:{action}></s:Body></s:Envelope>"#,
            escape(&service.urn)
        );
        let prefix = format!("{:03}-{}-{action}", self.records.len(), service.kind);
        tokio::fs::write(self.root.join(format!("{prefix}-request.xml")), &body).await?;
        record["request_file"] = json!(format!("{prefix}-request.xml"));
        let started = Instant::now();
        match self
            .client
            .post(service.control.clone())
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{}#{action}\"", service.urn))
            .body(body)
            .send()
            .await
        {
            Err(error) => {
                record["result"] = json!("request_error");
                record["error"] = json!(error.to_string());
            }
            Ok(response) => {
                let status = response.status();
                record["http_status"] = json!(status.as_u16());
                record["headers"] = json!(format!("{:?}", response.headers()));
                match response.bytes().await {
                    Err(error) => {
                        record["result"] = json!("body_error");
                        record["error"] = json!(error.to_string());
                    }
                    Ok(bytes) => {
                        let filename = format!("{prefix}-response.xml");
                        tokio::fs::write(self.root.join(&filename), &bytes).await?;
                        record["response_file"] = json!(filename);
                        match std::str::from_utf8(&bytes)
                            .map_err(anyhow::Error::from)
                            .and_then(|text| parse_reply(text, action))
                        {
                            Ok((fields, fault)) => {
                                record["fields"] = json!(fields);
                                record["result"] = json!(if fault.is_some() {
                                    "soap_fault"
                                } else if status.is_success() {
                                    "ok"
                                } else {
                                    "http_error"
                                });
                                if let Some(fault) = fault {
                                    record["fault"] = fault;
                                }
                            }
                            Err(error) => {
                                record["result"] = json!("invalid_response");
                                record["error"] = json!(error.to_string());
                            }
                        }
                    }
                }
            }
        }
        record["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
        self.records.push(record.clone());
        // Flush after each query, so interruption still leaves a useful report.
        tokio::fs::write(
            self.root.join("results.json"),
            serde_json::to_vec_pretty(&self.records)?,
        )
        .await?;
        Ok(record)
    }
}

fn instance_args(instance: u32) -> Vec<(&'static str, String)> {
    vec![("InstanceID", instance.to_string())]
}

/// Parse a UPnP time for diagnostics, preserving unavailable as None. Unlike a
/// clock, a media duration may exceed 24 hours and may contain fractional seconds.
fn seconds(value: &str) -> Option<f64> {
    let mut parts = value.trim().split(':');
    let hours = parts.next()?.parse::<u32>().ok()?;
    let minutes = parts.next()?.parse::<u32>().ok()?;
    let seconds = parts.next()?.parse::<f64>().ok()?;
    if parts.next().is_some()
        || minutes >= 60
        || !seconds.is_finite()
        || !(0.0..60.0).contains(&seconds)
    {
        return None;
    }
    Some(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds)
}

fn summary(records: &[Value]) -> String {
    let mut out = String::from(
        "只读 SOAP 诊断：未发送播放、暂停、定位、音量设置等控制命令。\n未公布的接口仅标记 not_advertised，不据此断言设备完全没有实现。\n\n",
    );
    for record in records {
        out.push_str(&format!(
            "{} {} {:?}: {} HTTP={}\n",
            record["service"],
            record["action"],
            record["args"],
            record["result"],
            record["http_status"]
        ));
        if let Some(fields) = record["fields"].as_object() {
            for (key, value) in fields {
                out.push_str(&format!("  {key}: {value}\n"));
            }
        }
        if !record["fault"].is_null() {
            out.push_str(&format!("  SOAP Fault: {}\n", record["fault"]));
        }
        if !record["error"].is_null() {
            out.push_str(&format!("  错误: {}\n", record["error"]));
        }
        if record["action"] == "GetPositionInfo" && record["result"] == "ok" {
            for key in ["RelTime", "AbsTime", "TrackDuration"] {
                let raw = record["fields"][key].as_str();
                out.push_str(&format!(
                    "  {key} 解析秒数: {:?}（None 表示缺失/不可用/非法，并非零）\n",
                    raw.and_then(seconds)
                ));
            }
        }
    }
    out.push_str("\n判读提示：\n- HTTP 200 且 actionResponse 正确，才标记 ok；HTTP 200 的 SOAP Fault 仍为失败。\n- 接口 ok 不等于字段有意义。NOT_IMPLEMENTED、空值或缺失不能当成真实的 0。\n- 连续 RelTime 递增说明设备会报告进度；保持不变需结合 PLAYING/PAUSED/STOPPED 判断。\n- TrackDuration=0 或不可用不等于无法查询当前位置；AbsTime 是位置，不是总时长。\n- 当前程序可能把无法解析的进度/音量显示成 0；以原始响应为准。\n- GetVolume 成功不证明 SetVolume 有效；此诊断不修改音量，无法验证设置是否生效。\n- Master/LF/RF 的音量值可能不同；结合实例编号和声道判断。\n- 401 通常表示动作无效；501 只表示执行失败，不能据此认定接口未实现。\n- 错误码需结合服务解释；AVTransport 701 可能是当前状态不允许，718 是实例编号无效。\n");
    out
}

/// Uses only description URLs and a fixed list of read-only SOAP actions. Each
/// request has a 3s deadline; discovered instance/connection IDs are bounded.
pub async fn save_read_only_diagnostics(location: &str, root: &Path) -> Result<PathBuf> {
    let description = tokio::fs::read_to_string(root.join("description.xml")).await?;
    let mut specs = services(&description, &Url::parse(location)?)?;
    let output = root.join(format!(
        "soap-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
    ));
    tokio::fs::create_dir_all(&output).await?;
    let mut recorder = Recorder {
        client: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()?,
        root: output.clone(),
        records: Vec::new(),
    };
    for (index, service) in specs.iter_mut().enumerate() {
        let result = async {
            let bytes = recorder
                .client
                .get(service.scpd.clone())
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            tokio::fs::write(output.join(format!("scpd-{index}.xml")), &bytes).await?;
            let doc = roxmltree::Document::parse(std::str::from_utf8(&bytes)?)?;
            service.actions = doc
                .descendants()
                .filter(|n| n.has_tag_name("action"))
                .filter_map(|n| {
                    n.children()
                        .find(|c| c.has_tag_name("name"))
                        .and_then(|c| c.text())
                        .map(str::to_owned)
                })
                .collect();
            service.channels = doc
                .descendants()
                .find(|n| {
                    n.has_tag_name("stateVariable")
                        && n.children().any(|c| {
                            c.has_tag_name("name") && c.text() == Some("A_ARG_TYPE_Channel")
                        })
                })
                .map(|n| {
                    n.descendants()
                        .filter(|c| c.has_tag_name("allowedValue"))
                        .filter_map(|c| c.text().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            recorder.records.push(json!({"service": service.kind, "action": "ReadSCPD", "result": "request_error", "error": error.to_string(), "url": service.scpd.as_str()}));
        }
    }
    let mut avt_ids = vec![0u32];
    let mut rcs_ids = vec![0u32];
    if let Some(cm) = specs.iter().find(|s| s.kind == "ConnectionManager") {
        recorder.query(cm, "GetProtocolInfo", &[]).await?;
        let ids = recorder.query(cm, "GetCurrentConnectionIDs", &[]).await?;
        if let Some(ids) = ids["fields"]["ConnectionIDs"].as_str() {
            for id in ids
                .split(',')
                .filter_map(|s| s.trim().parse::<i32>().ok())
                .take(3)
            {
                let info = recorder
                    .query(
                        cm,
                        "GetCurrentConnectionInfo",
                        &[("ConnectionID", id.to_string())],
                    )
                    .await?;
                for (key, values) in [("AVTransportID", &mut avt_ids), ("RcsID", &mut rcs_ids)] {
                    if let Some(id) = info["fields"][key]
                        .as_str()
                        .and_then(|v| v.parse::<u32>().ok())
                    {
                        if !values.contains(&id) {
                            values.push(id);
                        }
                    }
                }
            }
        }
    }
    if let Some(avt) = specs.iter().find(|s| s.kind == "AVTransport") {
        for id in &avt_ids {
            for action in [
                "GetDeviceCapabilities",
                "GetMediaInfo",
                "GetTransportSettings",
                "GetCurrentTransportActions",
            ] {
                recorder.query(avt, action, &instance_args(*id)).await?;
            }
        }
        println!("连续采样播放状态和进度（5轮，间隔2秒）；请保持设备原本的播放状态。");
        println!("同步读取 Master 音量/静音；可用原遥控器调一次音量，观察返回值是否变化。");
        for sample in 0..5 {
            if sample > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            for id in &avt_ids {
                for action in ["GetTransportInfo", "GetPositionInfo"] {
                    let reply = recorder.query(avt, action, &instance_args(*id)).await?;
                    println!(
                        "进度采样 {}/5 实例={} {}: {} {}",
                        sample + 1,
                        id,
                        action,
                        reply["result"],
                        reply["fields"]
                    );
                }
            }
            if let Some(rcs) = specs.iter().find(|s| s.kind == "RenderingControl") {
                for id in &rcs_ids {
                    let args = [
                        ("InstanceID", id.to_string()),
                        ("Channel", "Master".to_owned()),
                    ];
                    for action in ["GetVolume", "GetMute"] {
                        let reply = recorder.query(rcs, action, &args).await?;
                        println!(
                            "音量采样 {}/5 实例={} {}: {} {}",
                            sample + 1,
                            id,
                            action,
                            reply["result"],
                            reply["fields"]
                        );
                    }
                }
            }
        }
    }
    if let Some(rcs) = specs.iter().find(|s| s.kind == "RenderingControl") {
        for id in rcs_ids {
            recorder
                .query(rcs, "ListPresets", &instance_args(id))
                .await?;
            for channel in ["Master", "LF", "RF"] {
                if channel != "Master" && !rcs.channels.iter().any(|c| c == channel) {
                    continue;
                }
                let args = [
                    ("InstanceID", id.to_string()),
                    ("Channel", channel.to_owned()),
                ];
                for action in [
                    "GetVolume",
                    "GetMute",
                    "GetLoudness",
                    "GetVolumeDB",
                    "GetVolumeDBRange",
                ] {
                    if channel != "Master" && !["GetVolume", "GetMute"].contains(&action) {
                        continue;
                    }
                    let reply = recorder.query(rcs, action, &args).await?;
                    println!(
                        "音量诊断 实例={} 声道={} {}: {} {}",
                        id, channel, action, reply["result"], reply["fields"]
                    );
                }
            }
        }
    }
    tokio::fs::write(
        output.join("results.json"),
        serde_json::to_vec_pretty(&recorder.records)?,
    )
    .await?;
    tokio::fs::write(output.join("summary.txt"), summary(&recorder.records)).await?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_fields_preserve_unknown_and_fractional_long_durations() {
        assert_eq!(seconds("NOT_IMPLEMENTED"), None);
        assert_eq!(seconds(""), None);
        assert_eq!(seconds("00:00:00"), Some(0.0));
        assert_eq!(seconds("25:02:03.5"), Some(90123.5));
        assert_eq!(seconds("00:99:00"), None);
        assert_eq!(seconds("00:00:NaN"), None);
    }

    #[test]
    fn parses_namespaced_values_and_faults_without_zero_defaults() {
        let xml = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetPositionInfoResponse xmlns:u="urn:test"><u:RelTime>00:00:12.5</u:RelTime><u:TrackDuration>NOT_IMPLEMENTED</u:TrackDuration></u:GetPositionInfoResponse></s:Body></s:Envelope>"#;
        let (fields, fault) = parse_reply(xml, "GetPositionInfo").unwrap();
        assert!(fault.is_none());
        assert_eq!(fields["RelTime"], "00:00:12.5");
        assert!(!fields.contains_key("AbsTime"));
        assert!(parse_reply(xml, "GetVolume").is_err());
        let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><detail><UPnPError><errorCode>718</errorCode><errorDescription>Invalid InstanceID</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert_eq!(
            parse_reply(fault, "GetPositionInfo").unwrap().1.unwrap()["errorCode"],
            "718"
        );
    }

    #[test]
    fn service_urls_respect_urlbase_and_allowlist_excludes_controls() {
        let xml = "<root><URLBase>http://127.0.0.1:1234/base/</URLBase><device><serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>control</controlURL><SCPDURL>/scpd</SCPDURL></service></serviceList></device></root>";
        let specs = services(xml, &Url::parse("http://127.0.0.1:99/desc").unwrap()).unwrap();
        assert_eq!(
            specs[0].control.as_str(),
            "http://127.0.0.1:1234/base/control"
        );
        assert_eq!(specs[0].scpd.as_str(), "http://127.0.0.1:1234/scpd");
        for action in [
            "Play",
            "Pause",
            "Stop",
            "Seek",
            "SetVolume",
            "SetMute",
            "SelectPreset",
        ] {
            assert!(!READ_ACTIONS.contains(&action));
        }
    }
}
