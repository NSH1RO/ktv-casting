use async_trait::async_trait;
use std::net::IpAddr;

use super::{Capabilities, CastError, Caster, Progress, Quality, SongRef};
use crate::dlna_controller::{DlnaController, DlnaDevice};

/// How the renderer obtains media. Existing callers keep using the proxy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaUrlMode {
    Direct,
    #[default]
    Proxy,
}

impl std::str::FromStr for MediaUrlMode {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "direct" => Ok(Self::Direct),
            "" | "2" | "proxy" => Ok(Self::Proxy),
            _ => Err("请选择 1（原链接）或 2（反向代理）"),
        }
    }
}

fn transport_url(mode: MediaUrlMode, media: &str, ip: IpAddr, port: u16) -> String {
    match mode {
        MediaUrlMode::Direct => media.to_owned(),
        MediaUrlMode::Proxy => format!("http://{}/{}", std::net::SocketAddr::new(ip, port), media),
    }
}

async fn direct_media_url(song: &str) -> Result<String, CastError> {
    if song.starts_with("http://") || song.starts_with("https://") {
        return Ok(song.to_owned());
    }
    // The playlist normalizes bilibili://video/BV...?page=0 to BV...-page0.
    // Resolve the identifier to a single MP4 URL; the renderer downloads it.
    let path = song.split('?').next().unwrap_or(song);
    let (bvid, page) = match path.split_once("-page") {
        Some((bvid, page)) => (bvid, page.parse::<u32>()
            .map_err(|_| CastError::Device("无效的 Bilibili 分P编号".into()))?),
        None => (path, 0),
    };
    match crate::bilibili_parser::get_bilibili_media(bvid, Some(page), Quality::P720.as_qn())
        .await.map_err(|error| CastError::Device(format!("解析原链接失败：{error}")))? {
        crate::bilibili_parser::BilibiliMedia::Direct { url } => Ok(url),
        _ => Err(CastError::Device("双轨媒体需要本地混流，请选择反向代理".into())),
    }
}

pub struct DlnaCaster {
    controller: DlnaController,
    device: DlnaDevice,
    server_ip: IpAddr,
    server_port: u16,
    current_song: std::sync::Mutex<Option<String>>,
    sessions: Option<std::sync::Arc<crate::media_session::MediaSessions>>,
    media_client: reqwest::Client,
    transport: tokio::sync::Mutex<()>,
    seek_sequence: std::sync::atomic::AtomicU64,
    loaded_generation: std::sync::atomic::AtomicU64,
    media_url_mode: MediaUrlMode,
}

impl DlnaCaster {
    pub fn new(
        controller: DlnaController,
        device: DlnaDevice,
        server_ip: IpAddr,
        server_port: u16,
    ) -> Self {
        Self {
            controller,
            device,
            server_ip,
            server_port,
            current_song: std::sync::Mutex::new(None),
            sessions: None,
            media_client: reqwest::Client::builder()
                .connect_timeout(crate::PROXY_CONNECT_TIMEOUT)
                .build()
                .expect("valid media client"),
            transport: tokio::sync::Mutex::new(()),
            seek_sequence: std::sync::atomic::AtomicU64::new(0),
            loaded_generation: std::sync::atomic::AtomicU64::new(0),
            media_url_mode: MediaUrlMode::Proxy,
        }
    }

    pub fn with_sessions(
        mut self,
        sessions: std::sync::Arc<crate::media_session::MediaSessions>,
    ) -> Self {
        self.sessions = Some(sessions);
        self
    }

    pub fn with_media_url_mode(mut self, mode: MediaUrlMode) -> Self {
        self.media_url_mode = mode;
        self
    }

    async fn reload_song_at(
        &self,
        song: &str,
        position: u32,
        reason: &str,
    ) -> Result<(), CastError> {
        let quality = crate::get_dlna_quality();
        let session = self.sessions.as_ref()
            .filter(|_| self.media_url_mode == MediaUrlMode::Proxy)
            .map(|sessions| sessions.activate(song, quality));
        let media_url = if self.media_url_mode == MediaUrlMode::Direct {
            direct_media_url(song).await?
        } else if let Some(session) = &session {
            session
                .prepare(&self.media_client)
                .await
                .map_err(|error| self.failure("准备媒体", error))?;
            session.path()
        } else if quality == Quality::P1080 {
            with_start_offset(song, position)
        } else {
            song.to_owned()
        };
        let _transport = self.transport.lock().await;
        if session.as_ref().is_some_and(|s| s.is_stopped()) {
            return Err(CastError::Device("曲目准备已被新请求替代".into()));
        }
        let _ = self.controller.stop(&self.device).await;
        if session.as_ref().is_some_and(|s| s.is_stopped()) {
            return Err(CastError::Device("播放请求已过期".into()));
        }
        self.controller
            .set_avtransport_url(
                &self.device,
                &transport_url(self.media_url_mode, &media_url, self.server_ip, self.server_port),
                "",
            )
            .await
            .map_err(|error| self.failure("设置媒体地址", error))?;
        if session.as_ref().is_some_and(|s| s.is_stopped()) {
            return Err(CastError::Device("播放请求已过期".into()));
        }
        self.controller.play_for_cast(&self.device).await
            .map_err(|error| self.failure("发送播放指令", error))?;
        if let Some(session) = &session {
            self.loaded_generation
                .store(session.generation, std::sync::atomic::Ordering::Relaxed);
        }

        if (self.media_url_mode == MediaUrlMode::Direct || session.is_some() || quality == Quality::P720) && position > 0 {
            if let Err(error) = self.controller.seek(&self.device, position).await {
                log::warn!(target: "DLNA1080", "切换媒体后恢复进度失败: quality={}, position={}s, error={}", quality.label(), position, error);
                return Err(self.failure("恢复播放进度", error));
            }
        }
        log::info!(
            target: "DLNA1080",
            "DLNA 已重载媒体: reason={}, quality={}, start={}s",
            reason,
            quality.label(),
            position
        );
        Ok(())
    }

    fn failure(&self, step: &str, error: impl std::fmt::Display) -> CastError {
        let message = format!("[DLNA] {}失败：设备={}，设备地址={}，错误={}",
            step, self.device.friendly_name, self.device.location, error);
        log::warn!("{}", message);
        CastError::Device(message)
    }
}

fn with_start_offset(song: &str, position: u32) -> String {
    let (path, query) = song.split_once('?').unwrap_or((song, ""));
    let parameters: Vec<&str> = query
        .split('&')
        .filter(|parameter| !parameter.is_empty() && !parameter.starts_with("start="))
        .collect();
    let prefix = parameters.join("&");
    if prefix.is_empty() {
        format!("{path}?start={position}")
    } else {
        format!("{path}?{prefix}&start={position}")
    }
}

impl Drop for DlnaCaster {
    fn drop(&mut self) {
        if let Some(sessions) = &self.sessions {
            sessions.clear();
        }
    }
}

#[async_trait]
impl Caster for DlnaCaster {
    async fn play_song(&self, song: &SongRef) -> Result<(), CastError> {
        if let Ok(mut current) = self.current_song.lock() {
            *current = Some(song.0.clone());
        }
        self.seek_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.reload_song_at(&song.0, 0, "播放歌曲").await
    }

    async fn resume(&self) -> Result<(), CastError> {
        let _transport = self.transport.lock().await;
        self.controller.play(&self.device).await
            .map_err(|error| self.failure("继续播放", error))
    }

    async fn pause(&self) -> Result<(), CastError> {
        let _transport = self.transport.lock().await;
        self.controller.pause(&self.device).await
            .map_err(|error| self.failure("暂停", error))
    }

    async fn stop(&self) -> Result<(), CastError> {
        self.seek_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _transport = self.transport.lock().await;
        self.controller.stop(&self.device).await
            .map_err(|error| self.failure("停止播放", error))
    }

    async fn seek(&self, secs: u32) -> Result<(), CastError> {
        if self.media_url_mode == MediaUrlMode::Proxy && self.sessions.is_none() && crate::get_dlna_quality() == Quality::P1080 {
            let song = self
                .current_song
                .lock()
                .ok()
                .and_then(|current| current.clone())
                .ok_or_else(|| CastError::Device("DLNA 尚未加载歌曲，无法定位".to_string()))?;
            log::info!(target: "DLNA1080", "1080P 定位请求: {}s；通过 start 参数重开混流", secs);
            return self.reload_song_at(&song, secs, "1080P 定位").await;
        }
        let sequence = self
            .seek_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let _transport = self.transport.lock().await;
        if sequence
            != self
                .seek_sequence
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(());
        }
        let mut old_streams = None;
        if let Some(sessions) = self.sessions.as_ref().filter(|_| self.media_url_mode == MediaUrlMode::Proxy) {
            if let Some(session) = sessions
                .get(
                    self.loaded_generation
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
            {
                old_streams = session.seek_stream_cutoff();
                session.warm_seek(secs);
            } else {
                return Err(CastError::Device("新曲目正在准备，尚不能定位".into()));
            }
        }
        let started = std::time::Instant::now();
        let result = self.controller.seek(&self.device, secs).await
            .map_err(|error| self.failure("定位", error));
        if result.is_ok() {
            if let Some((cache, cutoff)) = old_streams {
                // Cancel only streams present before the accepted command.
                // A fast renderer may already have opened its new Range GET.
                cache.retire_streams(cutoff);
            }
        }
        log::info!(target: "DLNA1080", "原生 DLNA 定位指令返回: target={}s, command_ms={}, success={}（不代表画面已恢复）", secs, started.elapsed().as_millis(), result.is_ok());
        result
    }

    async fn get_progress(&self) -> Result<Progress, CastError> {
        self.controller
            .get_secs(&self.device)
            .await
            .map(|(curr, total)| Progress {
                current_secs: curr,
                total_secs: total,
            })
            .map_err(|error| self.failure("查询进度", error))
    }

    async fn set_volume(&self, volume: u32) -> Result<(), CastError> {
        self.controller
            .set_volume(&self.device, volume.clamp(0, 100))
            .await
            .map_err(|error| self.failure("设置音量", error))
    }

    async fn get_volume(&self) -> Result<Option<u32>, CastError> {
        self.controller
            .get_volume(&self.device)
            .await
            .map(Some)
            .map_err(|error| self.failure("查询音量", error))
    }

    async fn set_quality(&self, quality: Quality) -> Result<(), CastError> {
        if self.media_url_mode == MediaUrlMode::Direct && quality != Quality::P720 {
            return Err(CastError::Device("原链接模式不支持本地清晰度切换，请选择反向代理".into()));
        }
        let previous = crate::get_dlna_quality();
        if previous == quality {
            return Ok(());
        }
        log::info!(target: "DLNA1080", "切换 DLNA 清晰度: {} -> {}", previous.label(), quality.label());
        let position = self
            .controller
            .get_secs(&self.device)
            .await
            .ok()
            .map(|progress| progress.0)
            .unwrap_or(0);
        crate::set_dlna_quality(quality).map_err(|message| {
            log::error!(target: "DLNA1080", "拒绝 DLNA 清晰度切换: {}", message);
            CastError::Device(message.to_string())
        })?;

        // Re-open the same proxy URL so the renderer immediately requests the
        // newly selected representation. Keep the old position when possible.
        let song = self
            .current_song
            .lock()
            .ok()
            .and_then(|current| current.clone());
        if let Some(song) = song {
            self.reload_song_at(&song, position, "清晰度切换").await?;
        } else {
            log::info!(target: "DLNA1080", "DLNA 清晰度已保存，将在下一首生效: quality={}", quality.label());
        }
        Ok(())
    }

    fn get_quality(&self) -> Result<Quality, CastError> {
        Ok(crate::get_dlna_quality())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            absolute_volume: true,
            seek: true,
            hardware_progress: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{with_start_offset, transport_url, direct_media_url, MediaUrlMode};

    #[tokio::test]
    async fn direct_cast_sends_original_url_and_uses_native_seek_with_sessions_attached() {
        use super::{Caster, DlnaCaster, DlnaController, DlnaDevice, SongRef};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!("http://{}/description.xml", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let header_end = loop {
                    let size = stream.read(&mut buffer).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&buffer[..size]);
                    if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
                let length = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                }).unwrap_or(0);
                while bytes.len() < header_end + length {
                    let size = stream.read(&mut buffer).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&buffer[..size]);
                }
                let body = if headers.starts_with("GET ") {
                    r#"<root><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
<friendlyName>URL mode renderer</friendlyName><serviceList><service>
<serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
<serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
<SCPDURL>/service.xml</SCPDURL><controlURL>/control</controlURL>
<eventSubURL>/events</eventSubURL></service></serviceList></device></root>"#
                } else {
                    bodies.push(String::from_utf8(bytes[header_end..header_end + length].to_vec()).unwrap());
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body/></s:Envelope>"#
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            bodies
        });
        let device = rupnp::Device::from_url(location.parse().unwrap()).await.unwrap();
        let sessions = std::sync::Arc::new(crate::media_session::MediaSessions::default());
        let caster = DlnaCaster::new(DlnaController::new(), DlnaDevice {
            friendly_name: "URL mode renderer".into(), location, device, services: vec![],
        }, "127.0.0.1".parse().unwrap(), 0)
            .with_sessions(sessions)
            .with_media_url_mode(MediaUrlMode::Direct);
        let song = "https://cdn.example/video.mp4?Policy=a%2Fb&Signature=x=y#media";
        caster.play_song(&SongRef(song.into())).await.unwrap();
        caster.seek(42).await.unwrap();
        let bodies = tokio::time::timeout(std::time::Duration::from_secs(5), server).await.unwrap().unwrap();
        let set_uri = bodies.iter().find(|body| body.contains("<CurrentURI>")).unwrap();
        let xml = roxmltree::Document::parse(set_uri).unwrap();
        assert_eq!(xml.descendants().find(|node| node.has_tag_name("CurrentURI")).unwrap().text(), Some(song));
        assert!(bodies.last().unwrap().contains("<Target>00:00:42</Target>"));
        assert!(caster.set_quality(super::Quality::P1080).await.is_err());
    }

    #[test]
    fn parses_manual_mode_and_rejects_invalid_choice() {
        assert_eq!("1".parse(), Ok(MediaUrlMode::Direct));
        assert_eq!("direct".parse(), Ok(MediaUrlMode::Direct));
        assert_eq!("2".parse(), Ok(MediaUrlMode::Proxy));
        assert_eq!("".parse(), Ok(MediaUrlMode::Proxy));
        assert!("3".parse::<MediaUrlMode>().is_err());
    }

    #[tokio::test]
    async fn direct_urls_keep_signed_query_and_fragment_without_network() {
        let song = "https://cdn.example/video.m3u8?Policy=a%2Fb&Signature=x=y#media";
        let resolved = direct_media_url(song).await.unwrap();
        assert_eq!(transport_url(MediaUrlMode::Direct, &resolved, "127.0.0.1".parse().unwrap(), 8080), song);
    }

    #[test]
    fn proxy_urls_keep_absolute_sources_and_session_paths_inside_proxy() {
        let ip = "192.168.1.2".parse().unwrap();
        assert_eq!(transport_url(MediaUrlMode::Proxy, "https://cdn.example/v.mp4?x=1&y=2", ip, 8080), "http://192.168.1.2:8080/https://cdn.example/v.mp4?x=1&y=2");
        assert_eq!(transport_url(MediaUrlMode::Proxy, "__ktv_media/3.mp4", "::1".parse().unwrap(), 8080), "http://[::1]:8080/__ktv_media/3.mp4");
    }

    #[test]
    fn replaces_an_existing_start_offset() {
        assert_eq!(
            with_start_offset("BV1xx-page0?qn=80&start=12", 42),
            "BV1xx-page0?qn=80&start=42"
        );
    }
}
