use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

pub struct Playback {
    backend: Option<AudioBackend>,
    unavailable_reason: Option<String>,
    silent_position: Duration,
    silent_paused: bool,
    base_position: Duration,
    started_at: Option<Instant>,
    volume: f32,
    muted: bool,
}

struct AudioBackend {
    _device: MixerDeviceSink,
    player: Player,
}

impl Playback {
    pub fn new(volume: f32, force_silent: bool) -> Self {
        let volume = volume.clamp(0.0, 1.5);
        let (backend, unavailable_reason) = if force_silent {
            (None, Some("已通过 --no-audio 启用静音浏览模式".to_owned()))
        } else {
            match DeviceSinkBuilder::open_default_sink() {
                Ok(device) => {
                    let player = Player::connect_new(device.mixer());
                    player.set_volume(volume);
                    (
                        Some(AudioBackend {
                            _device: device,
                            player,
                        }),
                        None,
                    )
                }
                Err(error) => (None, Some(format!("音频设备不可用：{error}"))),
            }
        };

        Self {
            backend,
            unavailable_reason,
            silent_position: Duration::ZERO,
            silent_paused: true,
            base_position: Duration::ZERO,
            started_at: None,
            volume,
            muted: false,
        }
    }

    pub fn load(&mut self, path: &Path, start: Duration, paused: bool) -> Result<()> {
        self.silent_position = start;
        self.silent_paused = paused;
        self.base_position = start;
        self.started_at = (!paused).then(Instant::now);
        let Some(backend) = self.backend.as_mut() else {
            return Ok(());
        };

        let file = File::open(path).with_context(|| format!("无法打开音频：{}", path.display()))?;
        let mut decoder = Decoder::try_from(file).context("无法解码 FLAC")?;
        if !start.is_zero() {
            decoder.try_seek(start).context("无法恢复播放进度")?;
        }

        backend.player.clear();
        backend.player.append(decoder);
        if paused {
            backend.player.pause();
        } else {
            backend.player.play();
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        self.silent_position = Duration::ZERO;
        self.silent_paused = true;
        self.base_position = Duration::ZERO;
        self.started_at = None;
        if let Some(backend) = self.backend.as_ref() {
            backend.player.clear();
            backend.player.pause();
        }
    }

    pub fn toggle_pause(&mut self) {
        let Some(backend) = self.backend.as_ref() else {
            return;
        };
        if backend.player.is_paused() {
            backend.player.play();
            self.started_at = Some(Instant::now());
        } else {
            self.base_position = self.position();
            self.started_at = None;
            backend.player.pause();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.backend
            .as_ref()
            .map_or(self.silent_paused, |backend| backend.player.is_paused())
    }

    pub fn is_empty(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(|backend| backend.player.empty())
    }

    pub fn position(&self) -> Duration {
        if self.backend.is_none() {
            return self.silent_position;
        }
        self.started_at.map_or(self.base_position, |started_at| {
            self.base_position + started_at.elapsed()
        })
    }

    pub fn seek_relative(&mut self, seconds: i64, duration: Duration) -> Result<()> {
        let current = self.position().as_secs_f64();
        let target =
            Duration::from_secs_f64((current + seconds as f64).clamp(0.0, duration.as_secs_f64()));
        self.silent_position = target;
        self.base_position = target;
        if let Some(backend) = self.backend.as_ref() {
            backend
                .player
                .try_seek(target)
                .context("当前文件不支持跳转")?;
            self.started_at = (!backend.player.is_paused()).then(Instant::now);
        }
        Ok(())
    }

    pub fn set_volume(&mut self, value: f32) {
        self.volume = value.clamp(0.0, 1.5);
        if !self.muted
            && let Some(backend) = self.backend.as_ref()
        {
            backend.player.set_volume(self.volume);
        }
    }

    pub fn adjust_volume(&mut self, delta: f32) {
        self.set_volume(self.volume + delta);
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(backend) = self.backend.as_ref() {
            backend
                .player
                .set_volume(if self.muted { 0.0 } else { self.volume });
        }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn is_available(&self) -> bool {
        self.backend.is_some()
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_silent_mode_does_not_require_an_audio_device() {
        let playback = Playback::new(0.7, true);
        assert!(!playback.is_available());
        assert!(playback.unavailable_reason().is_some());
        assert!(playback.is_paused());
    }
}
