#[cfg(all(target_os = "linux", feature = "bridge"))]
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};
#[cfg(all(target_os = "linux", feature = "bridge"))]
use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::config::Config;

#[cfg(not(all(target_os = "linux", feature = "bridge")))]
pub async fn run(_config: Config) -> Result<()> {
    #[cfg(target_os = "linux")]
    let message = "a2a2 run requires a bridge build; rebuild with --features bridge";
    #[cfg(not(target_os = "linux"))]
    let message = "a2a2 run is Linux-only; deploy it to the Raspberry Pi";

    Err(anyhow!(message))
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
pub async fn run(config: Config) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Context;
    use liba2::{ClientBuilder, LiveAudioDecoder, LiveAudioSendError, LivePcmFrame};
    use tokio::sync::mpsc;
    use tracing::{error, info, warn};

    use crate::avrcp;
    use crate::bluetooth::{
        calculate_rms, start_capture, BluetoothError, CaptureConfig, SystemSetup,
    };
    use crate::{select_airplay_target, AirPlayTarget};

    let target = config
        .airplay_match
        .clone()
        .ok_or_else(|| anyhow!("set --airplay-match or A2A2_AIRPLAY_MATCH"))?;

    let setup = SystemSetup::check();
    if !setup.ready {
        for issue in setup.issues {
            if let Some(fix) = issue.fix_command {
                error!("{}; suggested fix: {}", issue.description, fix);
            } else {
                error!("{}", issue.description);
            }
        }
        return Err(anyhow!("Bluetooth audio stack is not ready"));
    }

    configure_bluetooth_adapter(&config.bt_alias, config.bt_class, config.discoverable)
        .context("configure Bluetooth adapter")?;
    let pairing_agent = start_bluetooth_pairing_agent().context("start Bluetooth pairing agent")?;
    let (button_tx, mut button_rx) = mpsc::unbounded_channel();
    let _button_thread = match crate::buttons::spawn(button_tx) {
        Ok(thread) => Some(thread),
        Err(error) => {
            warn!(%error, "GPIO buttons are unavailable");
            None
        }
    };

    let mut client = ClientBuilder::new()
        .render_delay_ms(config.render_delay_ms)
        .build()
        .context("create AirPlay client")?;

    info!(target, "discovering AirPlay target");
    let airplay_target = select_airplay_target(
        &client,
        &target,
        Duration::from_secs(config.airplay_discovery_secs),
    )
    .await?;
    let group_streaming = match &airplay_target {
        AirPlayTarget::Single(device) => {
            info!(
                name = %device.name,
                model = %device.model,
                address = ?device.socket_addr(),
                "connecting AirPlay target"
            );
            client
                .connect_with_pin(device, &config.pin)
                .await
                .context("connect to AirPlay target")?;
            false
        }
        AirPlayTarget::Group(devices) => {
            let names = devices
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            info!(members = %names, "connecting AirPlay group target");
            client
                .connect_group(devices)
                .await
                .context("connect to AirPlay group target")?;
            true
        }
    };
    let airplay_volume = config.airplay_volume.clamp(0.0, 1.0);
    info!(
        volume = airplay_volume,
        "setting fixed AirPlay output volume"
    );
    if let Err(error) = client.set_volume_linear(airplay_volume).await {
        warn!(%error, "failed to set initial AirPlay volume");
    }

    let bt_timeout = if config.bluetooth_wait_secs == 0 {
        Duration::from_secs(365 * 24 * 60 * 60)
    } else {
        Duration::from_secs(config.bluetooth_wait_secs)
    };
    info!("waiting for an A2DP source to connect");
    let bt_address = wait_for_bluealsa_capture_device(bt_timeout, &mut button_rx, &pairing_agent)
        .await
        .context("wait for Bluetooth A2DP source")?;
    info!(address = %bt_address, "Bluetooth source connected");

    let capture_config = if config.hd_audio {
        CaptureConfig::for_bluealsa_hd(&bt_address)
    } else {
        CaptureConfig::for_bluealsa(&bt_address)
    };
    let mut capture = start_capture(capture_config).context("start BlueALSA capture")?;

    let (sender, decoder) = LiveAudioDecoder::create_pair(44100, 2, 32);
    let shared = Arc::new(CaptureShared::default());
    let shared_for_thread = Arc::clone(&shared);

    let capture_thread = std::thread::Builder::new()
        .name("a2a2-bt-capture".into())
        .spawn(move || {
            let mut total_frames_sent = 0u64;
            info!("Bluetooth capture thread started");

            while !shared_for_thread.stop.load(Ordering::Relaxed) {
                match capture.recv_timeout(Duration::from_millis(50)) {
                    Ok(frame) => {
                        let rms = calculate_rms(&frame.samples);
                        shared_for_thread
                            .audio_level
                            .store((rms * 1000.0) as u32, Ordering::Relaxed);

                        let frames = frame.samples.len() / 2;
                        shared_for_thread
                            .samples
                            .fetch_add(frames as u64, Ordering::Relaxed);

                        let live_frame = LivePcmFrame::new(frame.samples, 2, 44100);

                        match sender.try_send(live_frame) {
                            Ok(()) => {
                                total_frames_sent += 1;
                                shared_for_thread
                                    .sent_frames
                                    .store(total_frames_sent, Ordering::Relaxed);
                            }
                            Err(LiveAudioSendError::Full) => {
                                shared_for_thread
                                    .dropped_frames
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            Err(LiveAudioSendError::Disconnected) => {
                                shared_for_thread.failed.store(true, Ordering::Relaxed);
                                warn!("AirPlay live audio sender disconnected");
                                break;
                            }
                            Err(error) => {
                                shared_for_thread.failed.store(true, Ordering::Relaxed);
                                warn!(%error, "AirPlay live audio send failed");
                                break;
                            }
                        }
                    }
                    Err(BluetoothError::Timeout) => {}
                    Err(error) => {
                        shared_for_thread.failed.store(true, Ordering::Relaxed);
                        error!(%error, "capture failed");
                        break;
                    }
                }
            }

            capture.stop();
            info!("Bluetooth capture thread stopped");
        })
        .context("spawn Bluetooth capture thread")?;

    if group_streaming {
        client
            .start_group_live_streaming_with_decoder(decoder)
            .await
            .context("start AirPlay group live stream")?;
    } else {
        client
            .start_live_streaming_with_decoder(decoder)
            .await
            .context("start AirPlay live stream")?;
    }

    let (media_tx, mut media_rx) = mpsc::unbounded_channel();
    let avrcp_task = avrcp::spawn_monitor(media_tx);

    let result = event_loop(
        &mut client,
        &shared,
        &mut media_rx,
        &mut button_rx,
        &pairing_agent,
        config.sync_bluetooth_volume,
    )
    .await;

    shared.stop.store(true, Ordering::Relaxed);
    join_capture_thread(capture_thread);
    avrcp_task.abort();
    if let Err(error) = client.stop().await {
        warn!(%error, "failed to stop AirPlay stream cleanly");
    }
    if let Err(error) = client.disconnect().await {
        warn!(%error, "failed to disconnect AirPlay session cleanly");
    }

    result
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn configure_bluetooth_adapter(alias: &str, bt_class: u32, discoverable: bool) -> Result<()> {
    use std::process::Command;

    run_bluetoothctl(["power", "on"])?;
    run_bluetoothctl(["system-alias", alias])?;
    set_bluetooth_class(bt_class);

    if discoverable {
        run_bluetoothctl(["discoverable-timeout", "0"])?;
        run_bluetoothctl(["discoverable", "on"])?;
        run_bluetoothctl(["pairable", "on"])?;
    }

    fn run_bluetoothctl<const N: usize>(args: [&str; N]) -> Result<()> {
        let output = Command::new("bluetoothctl").args(args).output()?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            Err(anyhow!("bluetoothctl failed: {detail}"))
        }
    }

    Ok(())
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn set_bluetooth_class(bt_class: u32) {
    use std::process::Command;

    use tracing::warn;

    let class = format!("0x{bt_class:06x}");
    let adapters = match Command::new("hciconfig").output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim_start().split_once(':').map(|(name, _)| name))
            .filter(|name| name.starts_with("hci"))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>(),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            warn!(%stderr, "failed to list Bluetooth adapters for class setup");
            return;
        }
        Err(error) => {
            warn!(%error, "hciconfig is unavailable; Bluetooth class was not set");
            return;
        }
    };

    for adapter in adapters {
        let output = Command::new("hciconfig")
            .args([adapter.as_str(), "class", class.as_str()])
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                warn!(%adapter, %class, %stderr, "failed to set Bluetooth adapter class");
            }
            Err(error) => warn!(%adapter, %class, %error, "failed to set Bluetooth adapter class"),
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn start_bluetooth_pairing_agent() -> Result<BluetoothPairingAgent> {
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::Context;
    use tracing::debug;

    let mut child = Command::new("script")
        .args([
            "--quiet",
            "--return",
            "--flush",
            "--echo",
            "never",
            "--command",
            "bluetoothctl",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn bluetoothctl through pseudo-terminal")?;

    let stdin = Arc::new(Mutex::new(
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("bluetoothctl stdin is unavailable"))?,
    ));
    let approvals = Arc::new(PairingApprovals::default());
    if let Some(stdout) = child.stdout.take() {
        spawn_bluetooth_agent_reader("stdout", stdout, Arc::clone(&stdin), Arc::clone(&approvals));
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_bluetooth_agent_reader("stderr", stderr, Arc::clone(&stdin), Arc::clone(&approvals));
    }

    write_bluetooth_agent_command(&stdin, "agent off\n")?;
    std::thread::sleep(Duration::from_millis(200));
    write_bluetooth_agent_command(&stdin, "agent DisplayYesNo\n")?;
    std::thread::sleep(Duration::from_millis(200));
    write_bluetooth_agent_command(&stdin, "default-agent\npairable on\ndiscoverable on\n")?;

    std::thread::sleep(Duration::from_millis(500));
    if let Some(status) = child.try_wait()? {
        return Err(anyhow!("bluetoothctl pairing agent exited early: {status}"));
    }

    debug!("Bluetooth pairing agent started; pairing prompts require GPIO approval");
    Ok(BluetoothPairingAgent {
        child,
        stdin,
        approvals,
    })
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn write_bluetooth_agent_command(
    stdin: &std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>,
    command: &str,
) -> Result<()> {
    use std::io::Write;

    let mut stdin = stdin
        .lock()
        .map_err(|_| anyhow!("bluetoothctl stdin lock is poisoned"))?;
    stdin.write_all(command.as_bytes())?;
    stdin.flush()?;
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn spawn_bluetooth_agent_reader<R>(
    stream: &'static str,
    mut reader: R,
    stdin: std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>,
    approvals: std::sync::Arc<PairingApprovals>,
) where
    R: std::io::Read + Send + 'static,
{
    let result = std::thread::Builder::new()
        .name(format!("a2a2-bt-agent-{stream}"))
        .spawn(move || {
            use std::io::ErrorKind;
            use std::sync::atomic::Ordering;

            use tracing::{debug, info, warn};

            let mut bytes = [0u8; 512];
            let mut transcript = String::new();
            let mut last_answered = approvals.answered.load(Ordering::Relaxed);

            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(size) => {
                        let answered = approvals.answered.load(Ordering::Relaxed);
                        if answered != last_answered {
                            transcript.clear();
                            last_answered = answered;
                        }

                        transcript.push_str(&String::from_utf8_lossy(&bytes[..size]));
                        if transcript.len() > 4096 {
                            let cutoff = transcript.len().saturating_sub(2048);
                            let keep_from = transcript
                                .char_indices()
                                .find(|(index, _)| *index >= cutoff)
                                .map(|(index, _)| index)
                                .unwrap_or(0);
                            transcript.drain(..keep_from);
                        }

                        if !bluetooth_agent_needs_yes(&transcript)
                            || approvals.pending.load(Ordering::Relaxed)
                        {
                            continue;
                        }

                        if approvals.is_open() {
                            match write_bluetooth_agent_command(&stdin, "yes\n") {
                                Ok(()) => {
                                    approvals.mark_answered();
                                    info!(stream = %stream, "approved Bluetooth pairing prompt");
                                    transcript.clear();
                                    last_answered = approvals.answered.load(Ordering::Relaxed);
                                }
                                Err(error) => {
                                    warn!(
                                        stream = %stream,
                                        %error,
                                        "failed to approve Bluetooth pairing prompt"
                                    );
                                    break;
                                }
                            }
                        } else {
                            approvals.pending.store(true, Ordering::Relaxed);
                            warn!(
                                stream = %stream,
                                "Bluetooth pairing prompt is waiting for SW3 long press approval"
                            );
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => {
                        debug!(stream = %stream, %error, "Bluetooth pairing agent reader stopped");
                        break;
                    }
                }
            }
        });

    if let Err(error) = result {
        tracing::warn!(
            stream = %stream,
            %error,
            "failed to start Bluetooth pairing agent reader"
        );
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn bluetooth_agent_needs_yes(transcript: &str) -> bool {
    let text = transcript.to_ascii_lowercase();
    text.contains("(yes/no") || text.contains("[yes/no") || text.contains("[y/n")
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
struct BluetoothPairingAgent {
    child: std::process::Child,
    stdin: std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>,
    approvals: std::sync::Arc<PairingApprovals>,
}

#[derive(Default)]
#[cfg(all(target_os = "linux", feature = "bridge"))]
struct PairingApprovals {
    approved_until_ms: AtomicU64,
    pending: AtomicBool,
    answered: AtomicUsize,
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
const PAIRING_APPROVAL_WINDOW_MS: u64 = 45_000;

#[cfg(all(target_os = "linux", feature = "bridge"))]
impl PairingApprovals {
    fn is_open(&self) -> bool {
        use std::sync::atomic::Ordering;

        now_millis() <= self.approved_until_ms.load(Ordering::Acquire)
    }

    fn open_window(&self, duration_ms: u64) -> u64 {
        use std::sync::atomic::Ordering;

        let expires_at = now_millis().saturating_add(duration_ms);
        self.approved_until_ms.store(expires_at, Ordering::Release);
        expires_at
    }

    fn mark_answered(&self) {
        use std::sync::atomic::Ordering;

        self.pending.store(false, Ordering::Release);
        self.answered.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
impl BluetoothPairingAgent {
    fn approve_next_pairing_prompt(&self) {
        use std::sync::atomic::Ordering;
        use tracing::{info, warn};

        let expires_at = self.approvals.open_window(PAIRING_APPROVAL_WINDOW_MS);

        if self.approvals.pending.swap(false, Ordering::AcqRel) {
            match write_bluetooth_agent_command(&self.stdin, "yes\n") {
                Ok(()) => {
                    self.approvals.mark_answered();
                    info!(
                        expires_at_ms = expires_at,
                        "approved pending Bluetooth pairing prompt and opened pairing window"
                    );
                }
                Err(error) => {
                    warn!(%error, "failed to approve pending Bluetooth pairing prompt");
                }
            }
        } else {
            info!(
                expires_at_ms = expires_at,
                "opened Bluetooth pairing approval window"
            );
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
impl Drop for BluetoothPairingAgent {
    fn drop(&mut self) {
        use tracing::debug;

        if let Ok(Some(_)) = self.child.try_wait() {
            return;
        }

        if self.child.kill().is_ok() {
            let _ = self.child.wait();
            debug!("Bluetooth pairing agent stopped");
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn wait_for_bluealsa_capture_device(
    timeout: std::time::Duration,
    button_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::buttons::ButtonCommand>,
    pairing_agent: &BluetoothPairingAgent,
) -> Result<String> {
    use std::process::Command;

    use tokio::time::{sleep, Instant};
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(address) =
            find_bluealsa_capture_address(&Command::new("bluealsa-aplay").arg("-l").output()?)
        {
            return Ok(address);
        }
        if let Some(address) =
            find_bluealsa_capture_address(&Command::new("bluealsa-aplay").arg("-L").output()?)
        {
            return Ok(address);
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for a BlueALSA A2DP capture device"
            ));
        }

        tokio::select! {
            _ = sleep(std::time::Duration::from_millis(500)) => {}
            Some(command) = button_rx.recv() => {
                apply_waiting_button_command(command, pairing_agent).await;
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn apply_waiting_button_command(
    command: crate::buttons::ButtonCommand,
    pairing_agent: &BluetoothPairingAgent,
) {
    use tracing::{info, warn};

    match command {
        crate::buttons::ButtonCommand::ApprovePairing => {
            pairing_agent.approve_next_pairing_prompt();
        }
        crate::buttons::ButtonCommand::Disconnect => {
            info!(?command, "GPIO button command");
            if let Err(error) = disconnect_connected_bluetooth_devices().await {
                warn!(%error, "Bluetooth disconnect failed");
            }
        }
        _ => {
            info!(
                ?command,
                "GPIO button command ignored until Bluetooth audio is connected"
            );
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn find_bluealsa_capture_address(output: &std::process::Output) -> Option<String> {
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut in_capture_section = false;

    for line in text.lines() {
        if line.contains("List of CAPTURE") {
            in_capture_section = true;
            continue;
        }
        if line.contains("List of PLAYBACK") {
            in_capture_section = false;
            continue;
        }

        if (in_capture_section || line.contains("PROFILE=a2dp"))
            && line.to_ascii_lowercase().contains("a2dp")
        {
            if let Some(address) = extract_mac_address(line) {
                return Some(address);
            }
        }
    }

    None
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn extract_mac_address(text: &str) -> Option<String> {
    for token in text.split(|ch: char| !(ch.is_ascii_hexdigit() || ch == ':')) {
        if token.len() == 17 && is_mac_address(token) {
            return Some(token.to_ascii_uppercase());
        }
    }

    None
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn is_mac_address(value: &str) -> bool {
    value.split(':').map(str::len).eq([2, 2, 2, 2, 2, 2])
        && value.chars().all(|ch| ch == ':' || ch.is_ascii_hexdigit())
}

#[derive(Default)]
#[cfg(all(target_os = "linux", feature = "bridge"))]
struct CaptureShared {
    stop: AtomicBool,
    failed: AtomicBool,
    audio_level: AtomicU32,
    samples: AtomicU64,
    sent_frames: AtomicU64,
    dropped_frames: AtomicU64,
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn event_loop(
    client: &mut liba2::AirPlayClient,
    shared: &Arc<CaptureShared>,
    media_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::avrcp::MediaCommand>,
    button_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::buttons::ButtonCommand>,
    pairing_agent: &BluetoothPairingAgent,
    sync_bluetooth_volume: bool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use anyhow::anyhow;
    use tokio::time::MissedTickBehavior;
    use tracing::{debug, info};

    let mut feedback_tick = tokio::time::interval(Duration::from_secs(2));
    feedback_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut stats_tick = tokio::time::interval(Duration::from_secs(5));
    stats_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested");
                return Ok(());
            }
            _ = feedback_tick.tick() => {
                if let Err(error) = client.send_feedback().await {
                    return Err(anyhow!("AirPlay feedback failed: {error}"));
                }
                if shared.failed.load(Ordering::Relaxed) {
                    return Err(anyhow!("Bluetooth capture failed"));
                }
            }
            _ = stats_tick.tick() => {
                debug!(
                    samples = shared.samples.load(Ordering::Relaxed),
                    sent_frames = shared.sent_frames.load(Ordering::Relaxed),
                    dropped_frames = shared.dropped_frames.load(Ordering::Relaxed),
                    level = shared.audio_level.load(Ordering::Relaxed),
                    "bridge stats"
                );
            }
            Some(command) = media_rx.recv() => {
                apply_media_command(client, command, sync_bluetooth_volume).await;
            }
            Some(command) = button_rx.recv() => {
                apply_button_command(client, command, pairing_agent).await;
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn apply_media_command(
    client: &mut liba2::AirPlayClient,
    command: crate::avrcp::MediaCommand,
    sync_bluetooth_volume: bool,
) {
    use tracing::debug;

    match command {
        crate::avrcp::MediaCommand::Play => {
            if let Err(error) = client.resume().await {
                debug!(%error, "resume ignored");
            }
        }
        crate::avrcp::MediaCommand::Pause => {
            if let Err(error) = client.pause().await {
                debug!(%error, "pause ignored");
            }
        }
        crate::avrcp::MediaCommand::Stop => {
            if let Err(error) = client.stop().await {
                debug!(%error, "stop ignored");
            }
        }
        crate::avrcp::MediaCommand::Volume(volume) => {
            if sync_bluetooth_volume {
                if let Err(error) = client.set_volume_linear(volume).await {
                    debug!(%error, volume, "volume ignored");
                }
            } else {
                debug!(volume, "Bluetooth absolute volume ignored");
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn apply_button_command(
    client: &mut liba2::AirPlayClient,
    command: crate::buttons::ButtonCommand,
    pairing_agent: &BluetoothPairingAgent,
) {
    use tracing::{debug, info, warn};

    info!(?command, "GPIO button command");

    match command {
        crate::buttons::ButtonCommand::EnterRewindMode => enter_bluez_rewind_mode().await,
        crate::buttons::ButtonCommand::EnterFastForwardMode => {
            enter_bluez_fast_forward_mode().await
        }
        crate::buttons::ButtonCommand::Previous => run_bluez_player_method("Previous").await,
        crate::buttons::ButtonCommand::Next => run_bluez_player_method("Next").await,
        crate::buttons::ButtonCommand::Pause => {
            run_bluez_player_method("Pause").await;
            if let Err(error) = client.pause().await {
                debug!(%error, "AirPlay pause ignored");
            }
        }
        crate::buttons::ButtonCommand::Disconnect => {
            if let Err(error) = disconnect_connected_bluetooth_devices().await {
                warn!(%error, "Bluetooth disconnect failed");
            }
        }
        crate::buttons::ButtonCommand::Play => {
            run_bluez_player_method("Play").await;
            if let Err(error) = client.resume().await {
                debug!(%error, "AirPlay resume ignored");
            }
        }
        crate::buttons::ButtonCommand::ApprovePairing => {
            pairing_agent.approve_next_pairing_prompt();
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn enter_bluez_rewind_mode() {
    run_bluez_player_method("Rewind").await;
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn enter_bluez_fast_forward_mode() {
    run_bluez_player_method("FastForward").await;
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn run_bluez_player_method(method: &str) {
    use tracing::warn;

    let Some(path) = find_bluez_media_player_path().await else {
        warn!(method, "no connected BlueZ media player found");
        return;
    };

    call_bluez_player_method(&path, method).await;
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn call_bluez_player_method(path: &str, method: &str) -> bool {
    use tracing::{debug, warn};

    let output = tokio::process::Command::new("busctl")
        .args([
            "--system",
            "call",
            "org.bluez",
            path,
            "org.bluez.MediaPlayer1",
            method,
        ])
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            debug!(method, path, "BlueZ media command sent");
            true
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(method, path, error = %stderr.trim(), "BlueZ media command failed");
            false
        }
        Err(error) => {
            warn!(method, %error, "failed to run busctl for BlueZ media command");
            false
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn find_bluez_media_player_path() -> Option<String> {
    let output = tokio::process::Command::new("busctl")
        .args(["--system", "tree", "org.bluez"])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let path = &line[line.find("/org/bluez")?..];
            if path.contains("/player") {
                Some(path.trim().to_string())
            } else {
                None
            }
        })
        .next()
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
async fn disconnect_connected_bluetooth_devices() -> Result<()> {
    use anyhow::Context;

    let output = tokio::process::Command::new("bluetoothctl")
        .args(["devices", "Connected"])
        .output()
        .await
        .context("list connected Bluetooth devices")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "bluetoothctl devices Connected failed: {}",
            stderr.trim()
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let addresses = text
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|value| is_mac_address(value))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if addresses.is_empty() {
        return Ok(());
    }

    for address in addresses {
        let output = tokio::process::Command::new("bluetoothctl")
            .args(["disconnect", &address])
            .output()
            .await
            .with_context(|| format!("disconnect Bluetooth device {address}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "bluetoothctl disconnect {address} failed: {}",
                stderr.trim()
            ));
        }
    }

    Ok(())
}

#[cfg(all(target_os = "linux", feature = "bridge"))]
fn join_capture_thread(thread: std::thread::JoinHandle<()>) {
    use tracing::warn;

    if thread.join().is_err() {
        warn!("Bluetooth capture thread panicked");
    }
}
