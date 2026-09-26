#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    Arc,
    mpsc::{self, Receiver},
};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, Color32, RichText, ViewportBuilder};
use rfd::{MessageButtons, MessageDialog, MessageLevel};

#[path = "../../src/file_locks.rs"]
mod file_locks;
mod model;
#[path = "../../src/path_template.rs"]
mod path_template;
mod resources;
mod ui_fonts;
mod uninstall_engine;
mod util;

use crate::uninstall_engine::{
    self as installer_engine, LockingProcessInfo, ProgressState, UninstallTarget,
};

enum UninstallPhase {
    BeforeUninstall,
    Uninstalling,
    AfterUninstall,
}

enum UninstallWorkerEvent {
    Progress(ProgressState),
    RequestTerminateConfirmation {
        action: String,
        processes: Vec<LockingProcessInfo>,
        response_tx: mpsc::Sender<bool>,
    },
    Failed(String),
    Completed,
}

struct UninstallerApp {
    app_name: String,
    target: Option<UninstallTarget>,
    phase: UninstallPhase,
    progress: u8,
    progress_detail: String,
    error_text: Option<String>,
    worker_rx: Option<Receiver<UninstallWorkerEvent>>,
    logo_texture: Option<egui::TextureHandle>,
    window_icon: Option<Arc<egui::IconData>>,
    window_icon_applied: bool,
    show_terminate_confirmation: bool,
    terminate_confirmation_action: String,
    terminate_confirmation_processes: Vec<LockingProcessInfo>,
    terminate_confirmation_response_tx: Option<mpsc::Sender<bool>>,
}

impl UninstallerApp {
    fn new() -> Self {
        let info = resources::installer_info().expect("failed to read info.json");
        let window_icon = resources::uninstaller_icon_data().ok().map(Arc::new);
        let resolved = installer_engine::resolve_uninstall_target(&info);
        match resolved {
            Ok(target) => Self {
                app_name: target.app_name.clone(),
                target: Some(target),
                phase: UninstallPhase::BeforeUninstall,
                progress: 0,
                progress_detail: "等待开始卸载".to_string(),
                error_text: None,
                worker_rx: None,
                logo_texture: None,
                window_icon: window_icon.clone(),
                window_icon_applied: false,
                show_terminate_confirmation: false,
                terminate_confirmation_action: String::new(),
                terminate_confirmation_processes: Vec::new(),
                terminate_confirmation_response_tx: None,
            },
            Err(error) => Self {
                app_name: info.display_name,
                target: None,
                phase: UninstallPhase::BeforeUninstall,
                progress: 0,
                progress_detail: "等待开始卸载".to_string(),
                error_text: Some(error.to_string()),
                worker_rx: None,
                logo_texture: None,
                window_icon,
                window_icon_applied: false,
                show_terminate_confirmation: false,
                terminate_confirmation_action: String::new(),
                terminate_confirmation_processes: Vec::new(),
                terminate_confirmation_response_tx: None,
            },
        }
    }

    fn ensure_logo_texture(&mut self, ctx: &egui::Context) {
        if self.logo_texture.is_some() {
            return;
        }
        let Ok(icon_data) = resources::app_logo_data() else {
            return;
        };
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [icon_data.width as usize, icon_data.height as usize],
            &icon_data.rgba,
        );
        let texture = ctx.load_texture(
            "uninstaller_panel_logo",
            color_image,
            egui::TextureOptions::LINEAR,
        );
        self.logo_texture = Some(texture);
    }

    fn ensure_window_icon(&mut self, ctx: &egui::Context) {
        if self.window_icon_applied {
            return;
        }
        if let Some(icon) = self.window_icon.clone() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(icon)));
        }
        self.window_icon_applied = true;
        ctx.request_repaint();
    }

    fn show_logo(&self, ui: &mut egui::Ui, size: f32) {
        if let Some(texture) = self.logo_texture.as_ref() {
            ui.add(egui::Image::from_texture(texture).fit_to_exact_size(egui::vec2(size, size)));
        }
    }

    fn start_uninstall(&mut self) {
        let Some(target) = self.target.clone() else {
            self.error_text = Some("安装程序未找到".to_owned());
            return;
        };
        self.phase = UninstallPhase::Uninstalling;
        self.progress = 0;
        self.progress_detail = "正在准备卸载".to_string();
        self.error_text = None;
        self.show_terminate_confirmation = false;
        self.terminate_confirmation_action.clear();
        self.terminate_confirmation_processes.clear();
        self.terminate_confirmation_response_tx = None;

        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        thread::spawn(move || {
            let progress_tx = tx.clone();
            let confirm_tx = tx.clone();
            let result = installer_engine::run_uninstall(
                &target,
                |state| {
                    let _ = progress_tx.send(UninstallWorkerEvent::Progress(state));
                },
                |processes| {
                    request_process_termination_confirmation("卸载", processes, &confirm_tx)
                },
            );
            match result {
                Ok(()) => {
                    let _ = tx.send(UninstallWorkerEvent::Completed);
                }
                Err(error) => {
                    let _ = tx.send(UninstallWorkerEvent::Failed(error.to_string()));
                }
            }
        });
    }

    fn finish_terminate_confirmation(&mut self, confirmed: bool) {
        if let Some(response_tx) = self.terminate_confirmation_response_tx.take() {
            let _ = response_tx.send(confirmed);
        }
        self.show_terminate_confirmation = false;
        self.terminate_confirmation_action.clear();
        self.terminate_confirmation_processes.clear();
    }

    fn poll_worker(&mut self) {
        let mut clear_receiver = false;
        if let Some(receiver) = self.worker_rx.as_ref() {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    UninstallWorkerEvent::Progress(state) => {
                        self.progress = state.percent;
                        self.progress_detail = state.detail;
                    }
                    UninstallWorkerEvent::RequestTerminateConfirmation {
                        action,
                        processes,
                        response_tx,
                    } => {
                        if let Some(prev_tx) = self.terminate_confirmation_response_tx.take() {
                            let _ = prev_tx.send(false);
                        }
                        self.show_terminate_confirmation = true;
                        self.terminate_confirmation_action = action;
                        self.terminate_confirmation_processes = processes;
                        self.terminate_confirmation_response_tx = Some(response_tx);
                    }
                    UninstallWorkerEvent::Failed(message) => {
                        self.phase = UninstallPhase::BeforeUninstall;
                        self.error_text = Some(message);
                        clear_receiver = true;
                    }
                    UninstallWorkerEvent::Completed => {
                        self.progress = 100;
                        self.progress_detail = "卸载完成".to_string();
                        self.phase = UninstallPhase::AfterUninstall;
                        clear_receiver = true;
                    }
                }
            }
        }
        if clear_receiver {
            self.worker_rx = None;
        }
    }
}

impl eframe::App for UninstallerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_window_icon(ctx);
        self.ensure_logo_texture(ctx);
        self.poll_worker();
        if matches!(self.phase, UninstallPhase::Uninstalling) || self.show_terminate_confirmation {
            ctx.request_repaint_after(Duration::from_millis(33));
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.phase {
            UninstallPhase::BeforeUninstall => {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    self.show_logo(ui, 96.0);
                    ui.add_space(15.0);
                    ui.label(RichText::new(&self.app_name).size(16.0));
                    ui.add_space(5.0);
                    let enabled = self.target.is_some();
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(RichText::new("卸载程序").color(Color32::WHITE))
                                .min_size([150.0, 40.0].into())
                                .fill(Color32::from_rgb(175, 28, 28)),
                        )
                        .clicked()
                    {
                        self.start_uninstall();
                    }
                    if let Some(error) = self.error_text.as_ref() {
                        ui.add_space(8.0);
                        ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                    }
                });
            }
            UninstallPhase::Uninstalling => {
                ui.vertical_centered(|ui| {
                    ui.add_space(130.0);
                    ui.heading("卸载中..");
                    ui.add_space(6.0);
                    ui.label(&self.progress_detail);
                    ui.add_space(10.0);
                    let finished = self.progress;
                    ui.add(
                        egui::ProgressBar::new(finished as f32 / 100.0)
                            .show_percentage()
                            .desired_width(300.0),
                    );
                });
            }
            UninstallPhase::AfterUninstall => {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    self.show_logo(ui, 96.0);
                    ui.add_space(15.0);
                    ui.label(RichText::new(&self.app_name).size(16.0));
                    ui.add_space(5.0);
                    if ui
                        .add_sized(
                            [150.0, 40.0],
                            egui::Button::new(RichText::new("完成卸载").color(Color32::WHITE))
                                .fill(Color32::from_rgb(175, 28, 28)),
                        )
                        .clicked()
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            }
        });

        if self.show_terminate_confirmation {
            let mut open = self.show_terminate_confirmation;
            let mut confirm = false;
            let mut cancel = false;

            egui::Window::new("确认终止进程")
                .collapsible(false)
                .resizable(true)
                .default_size([560.0, 360.0])
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "继续{}前将终止以下进程：",
                        self.terminate_confirmation_action
                    ));
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for process in &self.terminate_confirmation_processes {
                                ui.label(format!("{} (PID {})", process.name, process.pid));
                            }
                        });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                        if ui.button("继续").clicked() {
                            confirm = true;
                        }
                    });
                });

            if confirm {
                self.finish_terminate_confirmation(true);
            } else if cancel || !open {
                self.finish_terminate_confirmation(false);
            }
        }
    }
}

fn request_process_termination_confirmation(
    action: &str,
    processes: &[LockingProcessInfo],
    event_tx: &mpsc::Sender<UninstallWorkerEvent>,
) -> Result<bool> {
    if processes.is_empty() {
        return Ok(true);
    }

    let (response_tx, response_rx) = mpsc::channel();
    event_tx
        .send(UninstallWorkerEvent::RequestTerminateConfirmation {
            action: action.to_string(),
            processes: processes.to_vec(),
            response_tx,
        })
        .context("发送终止进程确认请求失败")?;
    response_rx.recv().context("终止进程确认响应通道已关闭")
}

fn run_silent_uninstall() -> Result<()> {
    let info = resources::installer_info()?;
    let target = installer_engine::resolve_uninstall_target(&info)?;
    installer_engine::run_uninstall(&target, |_| {}, |_| Ok(true))?;
    Ok(())
}

fn relaunch_from_temp() -> Result<bool> {
    if env::args().any(|arg| arg == "--from-temp") {
        return Ok(false);
    }

    let source = env::current_exe().context("读取卸载器路径失败")?;
    let temp_dir = env::temp_dir().join("ModernInstaller").join("uninstaller");
    fs::create_dir_all(&temp_dir).context("创建卸载器临时目录失败")?;
    let temp_exe = temp_dir.join(format!(
        "ModernInstaller.Uninstaller-{}.exe",
        std::process::id()
    ));
    fs::copy(&source, &temp_exe).context("复制卸载器到临时目录失败")?;

    let mut command = Command::new(&temp_exe);
    command.args(env::args_os().skip(1));
    command.arg("--from-temp");
    command.current_dir(&temp_dir);
    command.spawn().context("启动临时卸载器失败")?;
    Ok(true)
}

fn uninstaller_log_path() -> PathBuf {
    env::temp_dir()
        .join("ModernInstaller")
        .join("ModernInstaller.Uninstaller.log")
}

fn append_uninstaller_log(message: &str) {
    let log_path = uninstaller_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) else {
        return;
    };
    let _ = writeln!(file, "{:?} {message}", std::time::SystemTime::now());
}

fn run_gui_uninstall() -> Result<()> {
    append_uninstaller_log("starting GUI uninstaller");
    let mut renderer_errors = Vec::new();
    for renderer in [eframe::Renderer::Wgpu, eframe::Renderer::Glow] {
        append_uninstaller_log(&format!("trying renderer: {renderer:?}"));
        let app = UninstallerApp::new();
        let icon = resources::uninstaller_icon_data().context("failed to load uninstaller icon")?;
        let native_options = eframe::NativeOptions {
            viewport: ViewportBuilder::default()
                .with_title("ModernInstaller")
                .with_inner_size([600.0, 370.0])
                .with_resizable(false)
                .with_icon(icon),
            centered: true,
            renderer,
            ..Default::default()
        };
        match eframe::run_native(
            "ModernInstaller",
            native_options,
            Box::new(move |cc| {
                ui_fonts::apply_harmony_font(&cc.egui_ctx);
                Ok(Box::new(app))
            }),
        ) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let text = format!("{renderer:?}: {error}");
                append_uninstaller_log(&format!("renderer startup failed: {text}"));
                renderer_errors.push(text);
            }
        }
    }
    Err(anyhow!(
        "failed to create uninstaller window: {}",
        renderer_errors.join(" | ")
    ))
}

fn main() {
    panic::set_hook(Box::new(|panic_info| {
        append_uninstaller_log(&format!("panic: {panic_info}"));
        append_uninstaller_log(&format!(
            "backtrace:\n{}",
            std::backtrace::Backtrace::force_capture()
        ));
    }));

    let silent = env::args().any(|arg| arg == "--silent");
    let result = panic::catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        if relaunch_from_temp()? {
            return Ok(());
        }
        if silent {
            run_silent_uninstall()
        } else {
            run_gui_uninstall()
        }
    }));
    let error = match result {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.to_string(),
        Err(payload) => payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_else(|| "unknown panic payload".to_owned()),
    };
    append_uninstaller_log(&format!("uninstaller failed: {error}"));
    if !silent {
        let description = format!(
            "卸载器启动失败\n{error}\n\n日志文件:\n{}",
            uninstaller_log_path().display()
        );
        let _ = MessageDialog::new()
            .set_level(MessageLevel::Error)
            .set_title("ModernInstaller")
            .set_description(&description)
            .set_buttons(MessageButtons::Ok)
            .show();
    }
    std::process::exit(1);
}
