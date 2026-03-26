#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{self, Color32, RichText, ViewportBuilder, Widget};

use modern_installer_r::installer_engine::{self, ExistingInstall, InstallResult};
use modern_installer_r::model::InstallerInfo;
use modern_installer_r::{resources, ui_fonts, util};

enum InstallPhase {
    BeforeInstall,
    Installing,
    AfterInstall,
}

enum InstallWorkerEvent {
    Progress(u8),
    Failed(String),
    Completed(InstallResult),
}

struct InstallerApp {
    info: InstallerInfo,
    existing: ExistingInstall,
    install_path: String,
    agreed: bool,
    show_detail: bool,
    show_agreement: bool,
    phase: InstallPhase,
    progress: u8,
    error_text: Option<String>,
    worker_rx: Option<Receiver<InstallWorkerEvent>>,
    result: Option<InstallResult>,
    logo_texture: Option<egui::TextureHandle>,
}

impl InstallerApp {
    fn new(info: InstallerInfo) -> Self {
        let existing = installer_engine::read_existing_install(&info);
        let suggested_path = installer_engine::suggested_install_path(&info, &existing);
        Self {
            info,
            existing,
            install_path: suggested_path.to_string_lossy().to_string(),
            agreed: false,
            show_detail: false,
            show_agreement: false,
            phase: InstallPhase::BeforeInstall,
            progress: 0,
            error_text: None,
            worker_rx: None,
            result: None,
            logo_texture: None,
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
            "installer_panel_logo",
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

    fn is_update(&self) -> bool {
        installer_engine::is_update(&self.info, &self.existing)
    }

    fn validate_current(&self) -> Result<()> {
        installer_engine::validate_install(
            &self.info,
            &PathBuf::from(self.install_path.trim()),
            self.agreed,
            &self.existing,
        )
    }

    fn start_install(&mut self) {
        let install_path = PathBuf::from(self.install_path.trim());
        if let Err(error) = self.validate_current() {
            self.error_text = Some(error.to_string());
            return;
        }

        self.error_text = None;
        self.progress = 0;
        self.phase = InstallPhase::Installing;

        let info = self.info.clone();
        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        thread::spawn(move || {
            let result = installer_engine::run_install(&info, &install_path, |progress| {
                let _ = tx.send(InstallWorkerEvent::Progress(progress));
            });
            match result {
                Ok(done) => {
                    let _ = tx.send(InstallWorkerEvent::Completed(done));
                }
                Err(error) => {
                    let _ = tx.send(InstallWorkerEvent::Failed(error.to_string()));
                }
            }
        });
    }

    fn poll_worker(&mut self) {
        let mut clear_receiver = false;
        if let Some(receiver) = self.worker_rx.as_ref() {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    InstallWorkerEvent::Progress(value) => {
                        self.progress = value;
                    }
                    InstallWorkerEvent::Failed(message) => {
                        self.phase = InstallPhase::BeforeInstall;
                        self.error_text = Some(message);
                        clear_receiver = true;
                    }
                    InstallWorkerEvent::Completed(result) => {
                        self.progress = 100;
                        self.result = Some(result);
                        self.phase = InstallPhase::AfterInstall;
                        clear_receiver = true;
                    }
                }
            }
        }
        if clear_receiver {
            self.worker_rx = None;
        }
    }

    fn pick_folder(&mut self) {
        let Some(picked) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let next_path = if picked.exists() && util::path_has_any_content(&picked) {
            picked.join(&self.info.display_name)
        } else {
            picked
        };
        self.install_path = next_path.to_string_lossy().to_string();
    }

    fn launch_application(&mut self) {
        if let Some(done) = self.result.as_ref() {
            if let Err(error) =
                installer_engine::launch_application(&done.executable_path, &done.installed_path)
            {
                self.error_text = Some(format!("启动应用失败: {error}"));
            }
        }
    }
}

impl eframe::App for InstallerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_logo_texture(ctx);
        self.poll_worker();
        if matches!(self.phase, InstallPhase::Installing) {
            ctx.request_repaint_after(Duration::from_millis(33));
        }

        match self.phase {
            InstallPhase::BeforeInstall => {
                egui::TopBottomPanel::bottom("installer_before_agreement")
                    .resizable(false)
                    .exact_height(42.0)
                    .show(ctx, |ui| {
                        ui.add_space(2.0);
                        ui.horizontal_centered(|ui| {
                            ui.checkbox(&mut self.agreed, "我已阅读并同意");
                            if ui
                                .add(egui::Button::new("《用户协议》").frame(false))
                                .clicked()
                            {
                                self.show_agreement = true;
                            }
                        });
                    });
                egui::CentralPanel::default().show(ctx, |ui| {
                    let validation_error =
                        self.validate_current().err().map(|error| error.to_string());
                    let can_install = validation_error.is_none();
                    ui.add_space(60.0);
                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.y = 5.0;
                        self.show_logo(ui, 96.0);
                        ui.add_space(10.0);
                        ui.label(RichText::new(&self.info.display_name).size(16.0));
                        ui.add_space(5.0);

                        if !self.show_detail {
                            let button_label = if self.is_update() {
                                "安装更新"
                            } else {
                                "一键安装"
                            };
                            if ui
                                .add_enabled(
                                    can_install,
                                    egui::Button::new(button_label)
                                        .min_size(egui::vec2(150.0, 40.0)),
                                )
                                .clicked()
                            {
                                self.start_install();
                            }
                            if let Some(error) = validation_error.as_ref() {
                                ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                            }
                            ui.horizontal_centered(|ui| {
                                if let Some(old_version) = self.existing.installed_version.as_ref()
                                {
                                    ui.label(old_version.to_string());
                                } else {
                                    ui.label("");
                                }
                                if self.is_update() {
                                    ui.colored_label(Color32::from_rgb(235, 132, 42), ">");
                                    ui.colored_label(
                                        Color32::from_rgb(235, 132, 42),
                                        &self.info.display_version,
                                    );
                                }
                                ui.vertical_centered(|ui| {
                                    let show_detail = egui::Button::new(
                                        "更多安装选项",
                                    );
                                    if show_detail.ui(ui).clicked()
                                    {
                                        self.show_detail = true;
                                    }
                                });

                            });

                        } else {
                            ui.horizontal(|ui| {
                                // 1. 计算内容总宽度 (输入框 300 + 间距 + 按钮宽度约 44)
                                let content_width = 300.0 + ui.spacing().item_spacing.x + 44.0;

                                // 2. 计算左侧需要的空白间距
                                let gap = (ui.available_width() - content_width) / 2.0;

                                if gap > 0.0 {
                                    // 分配并占位，但不画任何东西
                                    ui.allocate_space(egui::vec2(gap, 0.0));
                                }

                                // 3. 放置实际组件
                                ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut self.install_path));
                                if ui.button("修改").clicked() {
                                    self.pick_folder();
                                }
                            });
                            ui.vertical_centered(|ui| {
                                if let Some(error) = validation_error.as_ref() {
                                    ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                                }
                                if ui
                                    .add_enabled(
                                        can_install,
                                        egui::Button::new("安装").min_size(egui::vec2(150.0, 40.0)),
                                    )
                                    .clicked()
                                {
                                    self.start_install();
                                }
                            });

                        }

                        if let Some(error) = self.error_text.as_ref() {
                            ui.add_space(8.0);
                            ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                        }
                    });
                });
            }
            InstallPhase::Installing => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(130.0);
                        ui.heading("安装中..");
                        ui.add_space(10.0);
                        ui.add(
                            egui::ProgressBar::new(self.progress as f32 / 100.0)
                                .show_percentage()
                                .desired_width(300.0),
                        );
                    });
                });
            }
            InstallPhase::AfterInstall => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        self.show_logo(ui, 96.0);
                        ui.add_space(15.0);
                        ui.label(RichText::new(&self.info.display_name).size(16.0));
                        ui.add_space(5.0);
                        ui.horizontal_centered(|ui| {
                            if ui
                                .add_sized([150.0, 40.0], egui::Button::new("完成安装"))
                                .clicked()
                            {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            if ui
                                .add_sized([150.0, 40.0], egui::Button::new("立即体验"))
                                .clicked()
                            {
                                self.launch_application();
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                        if let Some(error) = self.error_text.as_ref() {
                            ui.add_space(8.0);
                            ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                        }
                    });
                });
            }
        }

        if self.show_agreement {
            egui::Window::new("用户协议")
                .collapsible(false)
                .resizable(true)
                .default_size([560.0, 360.0])
                .open(&mut self.show_agreement)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(resources::agreement_text());
                    });
                });
        }
    }
}

fn run_silent_install() -> Result<()> {
    let info = resources::installer_info()?;
    let existing = installer_engine::read_existing_install(&info);
    let install_path = installer_engine::suggested_install_path(&info, &existing);
    installer_engine::validate_install(&info, &install_path, true, &existing)?;
    let result = installer_engine::run_install(&info, &install_path, |_| {})?;
    installer_engine::launch_application(&result.executable_path, &result.installed_path)?;
    Ok(())
}

fn main() -> eframe::Result {
    if env::args().any(|arg| arg == "--silent") {
        if let Err(error) = run_silent_install() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let info = resources::installer_info().expect("failed to load installer info");
    let installer_icon = resources::installer_icon_data().expect("failed to load installer icon");
    let app = InstallerApp::new(info);
    let native_options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("ModernInstaller")
            .with_inner_size([600.0, 370.0])
            .with_resizable(false)
            .with_icon(installer_icon),
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
