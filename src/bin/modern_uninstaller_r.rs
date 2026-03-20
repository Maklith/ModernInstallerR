#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{self, Color32, RichText, ViewportBuilder};

use modern_installer_r::installer_engine::{self, UninstallTarget};
use modern_installer_r::{resources, ui_fonts};

enum UninstallPhase {
    BeforeUninstall,
    Uninstalling,
    AfterUninstall,
}

enum UninstallWorkerEvent {
    Progress(u8),
    Failed(String),
    Completed,
}

struct UninstallerApp {
    app_name: String,
    target: Option<UninstallTarget>,
    phase: UninstallPhase,
    remaining_progress: u8,
    error_text: Option<String>,
    worker_rx: Option<Receiver<UninstallWorkerEvent>>,
    logo_texture: Option<egui::TextureHandle>,
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
                remaining_progress: 100,
                error_text: None,
                worker_rx: None,
                logo_texture: None,
            },
            Err(error) => Self {
                app_name: info.display_name,
                target: None,
                phase: UninstallPhase::BeforeUninstall,
                remaining_progress: 100,
                error_text: Some(error.to_string()),
                worker_rx: None,
                logo_texture: None,
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
        self.remaining_progress = 100;
        self.error_text = None;

        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        thread::spawn(move || {
            let result = installer_engine::run_uninstall(&target, |remaining| {
                let _ = tx.send(UninstallWorkerEvent::Progress(remaining));
            });
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

    fn poll_worker(&mut self) {
        let mut clear_receiver = false;
        if let Some(receiver) = self.worker_rx.as_ref() {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    UninstallWorkerEvent::Progress(value) => self.remaining_progress = value,
                    UninstallWorkerEvent::Failed(message) => {
                        self.phase = UninstallPhase::BeforeUninstall;
                        self.error_text = Some(message);
                        clear_receiver = true;
                    }
                    UninstallWorkerEvent::Completed => {
                        self.remaining_progress = 0;
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
        if matches!(self.phase, UninstallPhase::Uninstalling) {
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
                    ui.add_space(10.0);
                    let finished = 100 - self.remaining_progress;
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
    }
}

fn run_silent_uninstall() -> Result<()> {
    let info = resources::installer_info()?;
    let target = installer_engine::resolve_uninstall_target(&info)?;
    installer_engine::run_uninstall(&target, |_| {})?;
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
