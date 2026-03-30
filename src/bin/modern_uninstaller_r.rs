#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use eframe::egui::{self, Color32, RichText, ViewportBuilder};

use modern_installer_r::installer_engine::{
    self, LockingProcessInfo, ProgressState, UninstallTarget,
};
use modern_installer_r::{resources, ui_fonts};

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
    show_terminate_confirmation: bool,
    terminate_confirmation_action: String,
    terminate_confirmation_processes: Vec<LockingProcessInfo>,
    terminate_confirmation_response_tx: Option<mpsc::Sender<bool>>,
}

impl UninstallerApp {
    fn new() -> Self {
        let info = resources::installer_info().expect("failed to read info.json");
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

fn main() -> eframe::Result {
    if env::args().any(|arg| arg == "--silent") {
        if let Err(error) = run_silent_uninstall() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let app = UninstallerApp::new();
    let uninstaller_icon =
        resources::uninstaller_icon_data().expect("failed to load uninstaller icon");
    let native_options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("ModernInstaller")
            .with_inner_size([600.0, 370.0])
            .with_resizable(false)
            .with_icon(uninstaller_icon),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "ModernInstaller",
        native_options,
        Box::new(move |cc| {
            ui_fonts::apply_harmony_font(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
