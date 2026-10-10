use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tty7_core::mobile::admin::{self, Status};

use super::kit::{self, BtnKind, Tk, fs};
use super::*;

pub(crate) struct PairDevicesState {
    id: Uuid,
    status: Option<Status>,
    relay_url: Entity<InputState>,
    tls_pin: Entity<InputState>,
    dirty: bool,
    busy: bool,
    revision: u64,
    polling: Cell<bool>,
    poll_error: Option<String>,
    action_error: Option<String>,
    qr_error: Option<String>,
    qr_uri: Option<Arc<str>>,
    qr: Option<(Arc<gpui::Image>, u32)>,
}

impl PairDevicesState {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Tty7App>,
        subs: &mut Vec<Subscription>,
    ) -> Self {
        let relay_url = cx.new(|cx| InputState::new(window, cx));
        let tls_pin = cx.new(|cx| InputState::new(window, cx));
        for input in [&relay_url, &tls_pin] {
            subs.push(cx.subscribe_in(input, window, |this, _, event, _, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                if let Some(s) = this.active_settings_mut() {
                    let m = &mut s.mobile;
                    m.dirty = m.status.as_ref().is_none_or(|status| {
                        m.relay_url.read(cx).value().trim() != status.relay_url
                            || m.tls_pin.read(cx).value().trim()
                                != status.tls_fingerprint.as_deref().unwrap_or("")
                    });
                }
                cx.notify();
            }));
        }
        Self {
            id: Uuid::new_v4(),
            status: None,
            relay_url,
            tls_pin,
            dirty: false,
            busy: false,
            revision: 0,
            polling: Cell::new(false),
            poll_error: None,
            action_error: None,
            qr_error: None,
            qr_uri: None,
            qr: None,
        }
    }
}

enum MobileAction {
    Configure { relay_url: String, tls_pin: Option<String>, enabled: bool },
    Begin,
    Cancel,
    Approve(String),
    Reject(String),
    Revoke(String),
    Refresh,
}

impl MobileAction {
    fn perform(self) -> io::Result<Status> {
        match self {
            Self::Configure { relay_url, tls_pin, enabled } =>
                return admin::configure(relay_url, tls_pin, enabled),
            Self::Begin => { admin::begin_pairing()?; }
            Self::Cancel => admin::cancel_pairing()?,
            Self::Approve(request) => admin::approve(request)?,
            Self::Reject(request) => admin::reject(request)?,
            Self::Revoke(device) => admin::revoke(device)?,
            Self::Refresh => {}
        }
        admin::status()
    }
}

struct MobileSnapshot {
    status: Status,
    qr: io::Result<Option<(Arc<gpui::Image>, u32)>>,
}

fn now_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn qr_image(uri: &str) -> io::Result<(Arc<gpui::Image>, u32)> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)
        .map_err(io::Error::other)?;
    let modules = code.width();
    let scale = 4;
    let side = ((modules + 8) * scale) as u32;
    let mut image = image::GrayImage::from_pixel(side, side, image::Luma([255]));
    for y in 0..modules {
        for x in 0..modules {
            if code[(x, y)] == qrcode::types::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        image.put_pixel(((x + 4) * scale + dx) as u32,
                            ((y + 4) * scale + dy) as u32, image::Luma([0]));
                    }
                }
            }
        }
    }
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(image).write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(io::Error::other)?;
    Ok((Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes.into_inner())), side))
}

fn snapshot(status: Status, cached_uri: Option<Arc<str>>) -> MobileSnapshot {
    let qr = match status.pairing.as_ref() {
        Some(pairing) if pairing.expires_at > now_seconds()
            && cached_uri.as_deref() != Some(pairing.uri.as_str()) => qr_image(&pairing.uri).map(Some),
        _ => Ok(None),
    };
    MobileSnapshot { status, qr }
}

impl Tty7App {
    fn adopt_mobile_snapshot(
        &mut self,
        snapshot: MobileSnapshot,
        configured: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self.active_settings_mut() else { return; };
        let m = &mut s.mobile;
        // A configuration request may finish after the user has edited again.
        // Keep that newer draft rather than overwriting it with the response.
        let sync_inputs = m.status.is_none() || !m.dirty || (configured
            && m.relay_url.read(cx).value().trim() == snapshot.status.relay_url
            && m.tls_pin.read(cx).value().trim()
                == snapshot.status.tls_fingerprint.as_deref().unwrap_or(""));
        let relay_url = snapshot.status.relay_url.clone();
        let tls_pin = snapshot.status.tls_fingerprint.clone().unwrap_or_default();
        let pairing = snapshot.status.pairing.as_ref()
            .filter(|p| p.expires_at > now_seconds());
        if pairing.is_none() {
            m.qr = None;
            m.qr_uri = None;
        }
        match snapshot.qr {
            Ok(Some(image)) => {
                m.qr = Some(image);
                m.qr_uri = pairing.map(|p| Arc::from(p.uri.as_str()));
                m.qr_error = None;
            }
            Ok(None) => {
                if pairing.is_none() { m.qr_error = None; }
            }
            Err(error) => {
                m.qr = None;
                m.qr_uri = None;
                m.qr_error = Some(error.to_string());
            }
        }
        m.status = Some(snapshot.status);
        m.poll_error = None;
        if sync_inputs {
            m.dirty = false;
            if m.relay_url.read(cx).value().as_ref() != relay_url.as_str() {
                m.relay_url.update(cx, |input, cx| input.set_value(relay_url, window, cx));
            }
            if m.tls_pin.read(cx).value().as_ref() != tls_pin.as_str() {
                m.tls_pin.update(cx, |input, cx| input.set_value(tls_pin, window, cx));
            }
        }
    }

    fn start_mobile_polling(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.active_settings() else { return; };
        if s.mobile.polling.replace(true) { return; }
        let id = s.mobile.id;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let state = this.update(cx, |this, _| {
                    let s = this.active_settings()?;
                    if s.mobile.id != id || s.section != SettingsSection::PairDevices || s.search_active {
                        return None;
                    }
                    Some((!s.mobile.busy).then(|| (s.mobile.revision, s.mobile.qr_uri.clone())))
                });
                match state {
                    Ok(Some(Some((revision, cached_uri)))) => {
                        let result = cx.background_spawn(async move {
                            admin::status().map(|status| snapshot(status, cached_uri))
                        }).await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            if !this.active_settings().is_some_and(|s| s.mobile.id == id
                                && s.mobile.revision == revision && s.section == SettingsSection::PairDevices
                                && !s.search_active) { return; }
                            match result {
                                Ok(status) => this.adopt_mobile_snapshot(status, false, window, cx),
                                Err(error) => {
                                    if let Some(s) = this.active_settings_mut() {
                                        s.mobile.poll_error = Some(error.to_string());
                                    }
                                }
                            }
                            cx.notify();
                        });
                    }
                    Ok(Some(None)) => {}
                    _ => break,
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(s) = this.active_settings_mut().filter(|s| s.mobile.id == id) {
                    s.mobile.polling.set(false);
                    cx.notify();
                }
            });
        }).detach();
    }

    fn mobile_action(&mut self, action: MobileAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.active_settings_mut() else { return; };
        if s.mobile.busy { return; }
        let m = &mut s.mobile;
        m.busy = true;
        m.revision += 1;
        m.action_error = None;
        let id = m.id;
        let cached_uri = m.qr_uri.clone();
        let configured = matches!(&action, MobileAction::Configure { .. });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move {
                action.perform().map(|status| snapshot(status, cached_uri))
            }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(s) = this.active_settings_mut().filter(|s| s.mobile.id == id) else { return; };
                s.mobile.busy = false;
                match result {
                    Ok(status) => this.adopt_mobile_snapshot(status, configured, window, cx),
                    Err(error) => {
                        if let Some(s) = this.active_settings_mut() {
                            s.mobile.action_error = Some(error.to_string());
                        }
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    fn configure_mobile(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.active_settings() else { return; };
        let relay_url = s.mobile.relay_url.read(cx).value().trim().to_string();
        let pin = s.mobile.tls_pin.read(cx).value().trim().to_string();
        self.mobile_action(MobileAction::Configure {
            relay_url, tls_pin: (!pin.is_empty()).then_some(pin), enabled,
        }, window, cx);
    }

    fn set_mobile_enabled(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if enabled {
            self.configure_mobile(true, window, cx);
            return;
        }
        // Turning access off must still work with an unfinished/invalid draft.
        let Some(status) = self.active_settings().and_then(|s| s.mobile.status.as_ref()) else {
            return;
        };
        self.mobile_action(MobileAction::Configure {
            relay_url: status.relay_url.clone(),
            tls_pin: status.tls_fingerprint.clone(),
            enabled: false,
        }, window, cx);
    }

    pub(crate) fn render_settings_mobile_config(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.active_settings() else { return div().into_any_element(); };
        let m = &s.mobile;
        let enabled = m.status.as_ref().is_some_and(|s| s.enabled);
        let disabled = m.busy || m.status.is_none();
        let switch = kit::switch("mobile-enabled")
            .checked(enabled)
            .disabled(disabled)
            .on_click(cx.listener(|this, enabled: &bool, window, cx| {
                this.set_mobile_enabled(*enabled, window, cx);
            }))
            .into_any_element();
        let url = self.settings_text_input(&m.relay_url, FORM_FIELD_W, false, cx).into_any_element();
        let pin = self.settings_text_input(&m.tls_pin, FORM_FIELD_W, false, cx).into_any_element();
        let apply = kit::button("mobile-apply", t(L10nKey::SettingsMobileApply), BtnKind::Secondary)
            .disabled(disabled || !m.dirty)
            .on_click(cx.listener(move |this, _, window, cx| this.configure_mobile(enabled, window, cx)));
        let has_pairing = m.status.as_ref().is_some_and(|s| s.pairing.is_some());
        let begin = kit::button("mobile-begin", t(if has_pairing {
            L10nKey::SettingsMobileRegenerateQr
        } else { L10nKey::SettingsMobileGenerateQr }), BtnKind::Primary)
            .disabled(disabled || !enabled || m.dirty)
            .on_click(cx.listener(|this, _, window, cx| this.mobile_action(MobileAction::Begin, window, cx)));
        self.settings_group(None, None, [
            self.settings_row(t(L10nKey::SettingsMobileEnabled), t(L10nKey::SettingsMobileEnabledDesc), switch, cx).into_any_element(),
            self.settings_row(t(L10nKey::SettingsMobileRelayUrl), t(L10nKey::SettingsMobileRelayUrlDesc), url, cx).into_any_element(),
            self.settings_row(t(L10nKey::SettingsMobileTlsPin), t(L10nKey::SettingsMobileTlsPinDesc), pin, cx).into_any_element(),
            h_flex().py(px(12.)).justify_end().child(apply).into_any_element(),
            self.settings_row(t(L10nKey::SettingsNavPairDevices), t(L10nKey::SettingsMobilePairDesc), begin.into_any_element(), cx).into_any_element(),
        ], cx)
    }

    pub(crate) fn render_settings_mobile(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.start_mobile_polling(window, cx);
        let tk = Tk::of(cx);
        let Some(s) = self.active_settings() else { return div().into_any_element(); };
        let m = &s.mobile;
        let mut notices = v_flex().gap(px(8.)).text_size(fs(12.));
        let connection = match &m.status {
            None => L10nKey::SettingsMobileLoading,
            Some(status) if !status.enabled => L10nKey::SettingsMobileDisabled,
            Some(status) if status.connected => L10nKey::SettingsMobileConnected,
            Some(_) => L10nKey::SettingsMobileDisconnected,
        };
        notices = notices.child(h_flex().gap(px(12.)).justify_between()
            .child(div().text_color(tk.k6).child(t(connection)))
            .child(kit::button("mobile-refresh", t(L10nKey::SettingsMobileRefresh), BtnKind::Link)
                .disabled(m.busy)
                .on_click(cx.listener(|this, _, window, cx| this.mobile_action(MobileAction::Refresh, window, cx)))));
        for error in [m.action_error.as_ref(), m.poll_error.as_ref(), m.qr_error.as_ref(),
            m.status.as_ref().and_then(|s| s.error.as_ref())].into_iter().flatten() {
            notices = notices.child(div().text_color(tk.danger).child(error.clone()));
        }
        if m.busy { notices = notices.child(div().text_color(tk.k5).child(t(L10nKey::SettingsWorking))); }
        let mut groups = vec![notices.into_any_element(), self.render_settings_mobile_config(cx)];
        if let Some(pairing) = m.status.as_ref().and_then(|s| s.pairing.as_ref()) {
            let remaining = pairing.expires_at.saturating_sub(now_seconds());
            let mut qr = v_flex().gap(px(12.)).items_start();
            if remaining > 0 {
                qr = qr.child(div().text_size(fs(12.)).text_color(tk.k6).child(t(L10nKey::SettingsMobileScanQr)));
                if let Some((image, side)) = &m.qr {
                    qr = qr.child(gpui::img(image.clone()).size(px(*side as f32)).max_w_full()
                        .object_fit(gpui::ObjectFit::Contain));
                }
                qr = qr.child(div().text_size(fs(12.)).text_color(tk.k5)
                    .child(t_fmt(L10nKey::SettingsMobileExpires, &[("seconds", &remaining.to_string())])));
            } else {
                qr = qr.child(div().text_size(fs(12.)).text_color(tk.warn_text).child(t(L10nKey::SettingsMobileExpired)));
            }
            qr = qr.child(kit::button("mobile-cancel", t(L10nKey::Cancel), BtnKind::Link).disabled(m.busy)
                .on_click(cx.listener(|this, _, window, cx| this.mobile_action(MobileAction::Cancel, window, cx))));
            groups.push(qr.into_any_element());
        }
        if let Some(status) = &m.status {
            if !status.pending.is_empty() {
                let rows = status.pending.iter().map(|pending| {
                    let approve_id = pending.request_id.clone();
                    let reject_id = pending.request_id.clone();
                    v_flex().gap(px(8.)).py(px(12.))
                        .child(div().font_weight(FontWeight::MEDIUM).child(pending.device_name.clone()))
                        .child(div().text_size(fs(20.)).child(pending.code.clone()))
                        .child(div().text_size(fs(12.)).text_color(tk.k6)
                            .child(t_fmt(L10nKey::SettingsMobileVerifyCode, &[("code", &pending.code)])))
                        .child(h_flex().gap(px(16.))
                            .child(kit::button(SharedString::from(format!("mobile-approve-{approve_id}")),
                                t(L10nKey::SettingsMobileApprove), BtnKind::Primary).disabled(m.busy)
                                .on_click(cx.listener(move |this, _, window, cx| this.mobile_action(MobileAction::Approve(approve_id.clone()), window, cx))))
                            .child(kit::button(SharedString::from(format!("mobile-reject-{reject_id}")),
                                t(L10nKey::SettingsMobileReject), BtnKind::Danger).disabled(m.busy)
                                .on_click(cx.listener(move |this, _, window, cx| this.mobile_action(MobileAction::Reject(reject_id.clone()), window, cx)))))
                        .into_any_element()
                }).collect::<Vec<_>>();
                groups.push(self.settings_group(Some(t(L10nKey::SettingsMobilePending)), None, rows, cx));
            }
            let mut rows = status.devices.iter().map(|device| {
                let id = device.credential_id.clone();
                h_flex().gap(px(16.)).py(px(12.)).items_center().justify_between()
                    .child(v_flex().gap(px(4.)).min_w_0()
                        .child(div().child(device.name.clone()))
                        .child(div().text_size(fs(12.)).text_color(if device.connected { tk.ok } else { tk.k5 })
                            .child(t(if device.connected { L10nKey::SettingsMobileConnected } else { L10nKey::SettingsMobileDisconnected }))))
                    .child(kit::button(SharedString::from(format!("mobile-revoke-{id}")),
                        t(L10nKey::SettingsMobileRevoke), BtnKind::Danger).disabled(m.busy)
                        .on_click(cx.listener(move |this, _, window, cx| this.mobile_action(MobileAction::Revoke(id.clone()), window, cx))))
                    .into_any_element()
            }).collect::<Vec<_>>();
            if rows.is_empty() {
                rows.push(div().text_size(fs(12.)).text_color(tk.k5).child(t(L10nKey::SettingsMobileNoDevices)).into_any_element());
            }
            groups.push(self.settings_group(Some(t(L10nKey::SettingsMobileDevices)), None, rows, cx));
        }
        Self::settings_page(groups)
    }
}
