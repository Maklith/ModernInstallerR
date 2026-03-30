#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use eframe::egui::{self, Color32, RichText, ViewportBuilder};
use rfd::{MessageButtons, MessageDialog, MessageLevel};

use modern_installer_r::installer_engine::{
    self, ExistingInstall, InstallResult, LockingProcessInfo, ProgressState,
};
use modern_installer_r::model::InstallerInfo;
use modern_installer_r::{resources, ui_fonts, util};

enum InstallPhase {
    BeforeInstall,
    Installing,
    AfterInstall,
}

enum InstallWorkerEvent {
    Progress(ProgressState),
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
    progress_detail: String,
    error_text: Option<String>,
    worker_rx: Option<Receiver<InstallWorkerEvent>>,
    result: Option<InstallResult>,
    logo_texture: Option<egui::TextureHandle>,
    show_lock_confirmation: bool,
    pending_install_path: Option<PathBuf>,
    locked_file_count: usize,
    locking_processes_preview: Vec<LockingProcessInfo>,
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
            progress_detail: "等待开始安装".to_string(),
            error_text: None,
            worker_rx: None,
            result: None,
            logo_texture: None,
            show_lock_confirmation: false,
            pending_install_path: None,
            locked_file_count: 0,
            locking_processes_preview: Vec::new(),
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
    fn start_install(&mut self, install_path: PathBuf) {
        self.show_lock_confirmation = false;
        self.pending_install_path = None;
        self.locked_file_count = 0;
        self.locking_processes_preview.clear();

        self.error_text = None;
        self.progress = 0;
        self.progress_detail = "正在准备安装".to_string();
        self.phase = InstallPhase::Installing;

        let info = self.info.clone();
        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        thread::spawn(move || {
            let result = installer_engine::run_install(&info, &install_path, |state| {
                let _ = tx.send(InstallWorkerEvent::Progress(state));
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

    fn request_start_install(&mut self) {
        if self.show_lock_confirmation {
            return;
        }

        let install_path = PathBuf::from(self.install_path.trim());
        if let Err(error) = self.validate_current() {
            self.error_text = Some(error.to_string());
            return;
        }

        let (locked_files, locking_processes) = match installer_engine::find_lock_preview_for_install(
            &self.info,
            &install_path,
        ) {
            Ok(result) => result,
            Err(error) => {
                self.error_text = Some(error.to_string());
                return;
            }
        };

        if locked_files.is_empty() {
            self.start_install(install_path);
            return;
        }

        self.pending_install_path = Some(install_path);
        self.locked_file_count = locked_files.len();
        self.locking_processes_preview = locking_processes;
        self.show_lock_confirmation = true;
        self.error_text = None;
    }

    fn cancel_pending_install(&mut self) {
        self.show_lock_confirmation = false;
        self.pending_install_path = None;
        self.locked_file_count = 0;
        self.locking_processes_preview.clear();
        self.error_text = Some("安装已取消".to_string());
    }
    fn poll_worker(&mut self) {
        let mut clear_receiver = false;
        if let Some(receiver) = self.worker_rx.as_ref() {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    InstallWorkerEvent::Progress(state) => {
                        self.progress = state.percent;
                        self.progress_detail = state.detail;
                    }
                    InstallWorkerEvent::Failed(message) => {
                        self.phase = InstallPhase::BeforeInstall;
                        self.error_text = Some(message);
                        clear_receiver = true;
                    }
                    InstallWorkerEvent::Completed(result) => {
                        self.progress = 100;
                        self.progress_detail = "安装完成".to_string();
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
                    let validation_error = self
                        .validate_current()
                        .err()
                        .map(|error| error.to_string());

                    let can_install = validation_error.is_none();

                    ui.add_space(60.0);

                    // 整体纵向结构
                    ui.vertical_centered(|ui| {
                        ui.spacing_mut().item_spacing.y = 5.0;

                        self.show_logo(ui, 96.0);
                        ui.add_space(10.0);
                        ui.label(RichText::new(&self.info.display_name).size(16.0));
                        ui.add_space(5.0);
                    });

                    // 版本号：单独做一行水平居中
                    {
                        let old_text = self
                            .existing
                            .installed_version
                            .as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_default();

                        let show_update = self.is_update();
                        let new_text = self.info.display_version.clone();

                        ui.horizontal(|ui| {
                            let font_id = egui::TextStyle::Body.resolve(ui.style());
                            let normal_color = ui.visuals().text_color();
                            let accent_color = Color32::from_rgb(235, 132, 42);

                            let old_width = if old_text.is_empty() {
                                0.0
                            } else {
                                ui.painter()
                                    .layout_no_wrap(old_text.clone(), font_id.clone(), normal_color)
                                    .size()
                                    .x
                            };

                            let arrow_width = if show_update {
                                ui.painter()
                                    .layout_no_wrap(">".to_owned(), font_id.clone(), accent_color)
                                    .size()
                                    .x
                            } else {
                                0.0
                            };

                            let new_width = if show_update {
                                ui.painter()
                                    .layout_no_wrap(new_text.clone(), font_id.clone(), accent_color)
                                    .size()
                                    .x
                            } else {
                                0.0
                            };

                            let spacing = ui.spacing().item_spacing.x;

                            let total_width = if show_update {
                                let mut w = 0.0;
                                if !old_text.is_empty() {
                                    w += old_width + spacing;
                                }
                                w += arrow_width + spacing + new_width;
                                w
                            } else {
                                old_width
                            };

                            let gap = ((ui.available_width() - total_width) / 2.0).max(0.0);
                            ui.add_space(gap);

                            if !old_text.is_empty() {
                                ui.label(old_text);
                            }

                            if show_update {
                                ui.colored_label(accent_color, ">");
                                ui.colored_label(accent_color, new_text);
                            }
                        });
                    }

                    if !self.show_detail {
                        ui.vertical_centered(|ui| {
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
                                self.request_start_install();
                            }

                            if let Some(error) = validation_error.as_ref() {
                                ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                            }

                            if ui.button("更多安装选项").clicked() {
                                self.show_detail = true;
                            }
                        });
                    } else {
                        // 安装路径：单独做一行水平居中
                        ui.horizontal(|ui| {
                            let content_width = 300.0 + ui.spacing().item_spacing.x + 44.0;
                            let gap = ((ui.available_width() - content_width) / 2.0).max(0.0);

                            ui.add_space(gap);

                            ui.add_sized(
                                [300.0, 20.0],
                                egui::TextEdit::singleline(&mut self.install_path),
                            );

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
                                    egui::Button::new("安装")
                                        .min_size(egui::vec2(150.0, 40.0)),
                                )
                                .clicked()
                            {
                                self.request_start_install();
                            }
                        });
                    }

                    if let Some(error) = self.error_text.as_ref() {
                        ui.vertical_centered(|ui| {
                            ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                        });
                    }
                });
            }
            InstallPhase::Installing => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(130.0);
                        ui.heading("安装中..");
                        ui.add_space(6.0);
                        ui.label(&self.progress_detail);
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
                            ui.horizontal(|ui| {
                                if ui.add_sized([150.0, 40.0], egui::Button::new("完成安装")).clicked() {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }

                                // 按钮之间的间距由 ui.spacing().item_spacing 自动处理

                                if ui.add_sized([150.0, 40.0], egui::Button::new("立即体验")).clicked() {
                                    self.launch_application();
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            });
                        });



                        if let Some(error) = self.error_text.as_ref() {
                            ui.add_space(8.0);
                            ui.colored_label(Color32::from_rgb(196, 20, 20), error);
                        }
                    });
                });
            }
        }

        if self.show_lock_confirmation {
            let mut open = self.show_lock_confirmation;
            let mut confirm_install = false;
            let mut cancel_install = false;

            egui::Window::new("检测到文件占用")
                .collapsible(false)
                .resizable(true)
                .default_size([560.0, 360.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "检测到本次安装涉及目录中有 {} 个文件被占用。",
                        self.locked_file_count
                    ));
                    ui.label("继续安装将尝试结束相关进程。");
                    ui.add_space(8.0);

                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for process in &self.locking_processes_preview {
                                ui.label(format!("{} (PID {})", process.name, process.pid));
                            }
                        });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("取消安装").clicked() {
                            cancel_install = true;
                        }
                        if ui.button("继续安装").clicked() {
                            confirm_install = true;
                        }
                    });
                });

            if confirm_install {
                if let Some(path) = self.pending_install_path.take() {
                    self.start_install(path);
                } else {
                    self.cancel_pending_install();
                }
            } else if cancel_install || !open {
                self.cancel_pending_install();
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

fn installer_log_path() -> PathBuf {
    env::temp_dir()
        .join("ModernInstaller")
        .join("ModernInstaller.log")
}

fn append_installer_log(message: &str) {
    let log_path = installer_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) else {
        return;
    };
    let _ = writeln!(file, "{:?} {message}", std::time::SystemTime::now());
}

fn report_startup_failure(context: &str, error: &str, show_dialog: bool) {
    append_installer_log(&format!("{context}: {error}"));
    if !show_dialog {
        return;
    }

    let log_path = installer_log_path();
    let description = format!("{context}\n{error}\n\n日志文件:\n{}", log_path.display());
    let _ = MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title("ModernInstaller")
        .set_description(&description)
        .set_buttons(MessageButtons::Ok)
        .show();
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
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

fn run_gui_install() -> Result<()> {
    append_installer_log("starting GUI installer");

    let info = resources::installer_info().context("failed to load installer info")?;
    let mut renderer_errors = Vec::new();

    for renderer in [eframe::Renderer::Wgpu, eframe::Renderer::Glow] {
        append_installer_log(&format!("trying renderer: {renderer:?}"));
        let installer_icon =
            resources::installer_icon_data().context("failed to load installer icon")?;
        let app = InstallerApp::new(info.clone());
        let native_options = eframe::NativeOptions {
            viewport: ViewportBuilder::default()
                .with_title("ModernInstaller")
                .with_inner_size([600.0, 370.0])
                .with_resizable(false)
                .with_icon(installer_icon),
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
                append_installer_log(&format!("renderer startup failed: {text}"));
                renderer_errors.push(text);
            }
        }
    }

    Err(anyhow!(
        "failed to create installer window: {}",
        renderer_errors.join(" | ")
    ))
}

fn main() {
    panic::set_hook(Box::new(|panic_info| {
        append_installer_log(&format!("panic: {panic_info}"));
        let backtrace = std::backtrace::Backtrace::force_capture();
        append_installer_log(&format!("backtrace:\n{backtrace}"));
    }));

    if env::args().any(|arg| arg == "--silent") {
        match panic::catch_unwind(AssertUnwindSafe(run_silent_install)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                report_startup_failure("silent install failed", &error.to_string(), false);
                eprintln!("{error}");
                std::process::exit(1);
            }
            Err(payload) => {
                let panic_message = panic_payload_to_string(payload);
                report_startup_failure("silent install panicked", &panic_message, false);
                eprintln!("{panic_message}");
                std::process::exit(1);
            }
        }
        return;
    }

    match panic::catch_unwind(AssertUnwindSafe(run_gui_install)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            report_startup_failure("installer startup failed", &error.to_string(), true);
            std::process::exit(1);
        }
        Err(payload) => {
            let panic_message = panic_payload_to_string(payload);
            report_startup_failure("installer startup panicked", &panic_message, true);
            std::process::exit(1);
        }
    }
}
