use crate::{clipboard::auto_paste, clipboard::press_enter, config::Config};
use eframe::{
    App, NativeOptions,
    egui::{self, vec2},
};
use rust_i18n::t;
use std::time::{Duration, Instant};
use winit::platform::macos::EventLoopBuilderExtMacOS;

const WINDOW_SIZE: egui::Vec2 = egui::Vec2::new(196.0, 146.0);
const CLOSE_BUTTON_SIZE: f32 = 12.0;
const CLOSE_BUTTON_OFFSET: egui::Vec2 = egui::Vec2::new(-4.0, -4.0);
const CONTENT_OFFSET: egui::Vec2 = egui::Vec2::new(2.0, 2.0);

pub struct VerificationCodeApp {
    code: String,
    source: String,
    created_at: Instant,
    lifetime: Duration,
    should_close: bool,
}

impl VerificationCodeApp {
    pub fn new(code: String, source: String) -> Self {
        Self {
            code,
            source,
            created_at: Instant::now(),
            lifetime: Duration::from_secs(600),
            should_close: false,
        }
    }

    pub fn run(code: String, source: String) {
        let options = NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size(WINDOW_SIZE)
                .with_resizable(false)
                .with_titlebar_shown(false)
                .with_titlebar_buttons_shown(false)
                .with_fullsize_content_view(true)
                .with_title_shown(false)
                .with_always_on_top(),
            event_loop_builder: Some(Box::new(|builder| {
                builder
                    .with_activation_policy(winit::platform::macos::ActivationPolicy::Prohibited);
            })),
            ..Default::default()
        };

        eframe::run_native(
            "VerificationCode",
            options,
            Box::new(|cc| {
                let mut fonts = egui::FontDefinitions::default();

                fonts.font_data.insert(
                    "PingFang SC".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                        "../../resources/PingFang-SC-Regular.ttf"
                    ))),
                );

                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .insert(0, "PingFang SC".to_owned());

                fonts
                    .families
                    .get_mut(&egui::FontFamily::Monospace)
                    .unwrap()
                    .insert(0, "PingFang SC".to_owned());

                cc.egui_ctx.set_fonts(fonts);
                Ok(Box::new(Self::new(code, source)))
            }),
        )
        .unwrap();
    }

    fn handle_window_drag(&self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let response = ui.interact(
            ui.max_rect(),
            ui.id().with("drag_window"),
            egui::Sense::drag(),
        );
        if response.dragged() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }

    fn draw_close_button(&self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let close_btn_pos = ui.max_rect().left_top() + CLOSE_BUTTON_OFFSET;
        let close_btn_rect = egui::Rect::from_center_size(
            close_btn_pos + vec2(CLOSE_BUTTON_SIZE / 2.0, CLOSE_BUTTON_SIZE / 2.0),
            vec2(CLOSE_BUTTON_SIZE, CLOSE_BUTTON_SIZE),
        );
        let close_btn_response = ui.allocate_rect(close_btn_rect, egui::Sense::click());

        let button_color = if close_btn_response.is_pointer_button_down_on() {
            egui::Color32::from_rgb(220, 90, 80)
        } else {
            egui::Color32::from_rgb(237, 106, 94)
        };

        ui.painter().circle_filled(
            close_btn_rect.center(),
            CLOSE_BUTTON_SIZE / 2.0,
            button_color,
        );

        if close_btn_response.hovered() {
            let text = "❌";
            let font = egui::FontId::proportional(CLOSE_BUTTON_SIZE * 0.9);
            let color = egui::Color32::from_rgb(152, 0, 0);

            let galley = ui
                .painter()
                .layout_no_wrap(text.to_owned(), font.clone(), color);

            let text_rect = egui::Rect::from_center_size(close_btn_rect.center(), galley.size());

            ui.painter().text(
                text_rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                font,
                color,
            );
        }

        if close_btn_response.clicked() {
            let ctx_clone = ctx.clone();
            std::thread::spawn(move || {
                ctx_clone.send_viewport_cmd(egui::ViewportCommand::Close);
            });
        }
    }

    fn draw_content(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        let content_area = ui.max_rect().translate(CONTENT_OFFSET);
        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(content_area)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );

        content_ui.add_space(10.0);

        let btn_response = self.custom_button(&mut content_ui);

        if btn_response.clicked() {
            let _ = auto_paste(true, &self.code);

            if let Ok(config) = Config::load() {
                if config.auto_enter {
                    if let Err(e) = press_enter() {
                        log::error!(
                            "{}",
                            t!("monitor.failed_to_press_enter_floating", error = e)
                        );
                    } else {
                        log::info!("{}", t!("monitor.auto_pressed_enter_floating"));
                    }
                }
            }

            self.should_close = true;
        }
    }

    fn formatted_code(&self) -> String {
        let chars: Vec<char> = self.code.chars().collect();
        if chars.len() >= 4 && chars.len() <= 8 && chars.iter().all(|c| c.is_ascii_digit()) {
            chars
                .into_iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            self.code.clone()
        }
    }

    fn custom_button(&self, ui: &mut egui::Ui) -> egui::Response {
        let available_size = ui.available_size();
        let group_size = vec2(available_size.x - 10.0, available_size.y - 6.0);

        let (rect, _) = ui.allocate_exact_size(group_size, egui::Sense::hover());
        let card_rect = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 2.0, rect.min.y + 18.0),
            egui::pos2(rect.max.x - 2.0, rect.max.y - 18.0),
        );
        let response = ui.interact(card_rect, ui.id().with("otp_card"), egui::Sense::click());

        let bg_color = if response.is_pointer_button_down_on() {
            if ui.visuals().dark_mode {
                egui::Color32::from_rgb(0x3C, 0x3C, 0x3C)
            } else {
                egui::Color32::from_rgb(0xE6, 0xE6, 0xE6)
            }
        } else if response.hovered() {
            // 悬停时的颜色
            if ui.visuals().dark_mode {
                egui::Color32::from_rgb(0x45, 0x45, 0x45)
            } else {
                egui::Color32::from_rgb(0xD8, 0xD8, 0xD8)
            }
        } else {
            // 正常状态的颜色
            if ui.visuals().dark_mode {
                egui::Color32::from_rgb(0x3C, 0x3C, 0x3C)
            } else {
                egui::Color32::from_rgb(0xE6, 0xE6, 0xE6)
            }
        };

        ui.painter().rect_filled(
            card_rect.translate(vec2(0.0, 3.0)),
            6.0,
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 18),
        );
        ui.painter().rect_filled(card_rect, 6.0, bg_color);
        ui.painter().rect_stroke(
            card_rect,
            6.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 45)),
            egui::StrokeKind::Inside,
        );

        let primary_text_color = if ui.visuals().dark_mode {
            egui::Color32::WHITE
        } else {
            egui::Color32::BLACK
        };
        let source_text_color = if ui.visuals().dark_mode {
            egui::Color32::from_rgba_unmultiplied(210, 210, 210, 135)
        } else {
            egui::Color32::from_rgba_unmultiplied(78, 78, 78, 150)
        };
        let hint_text_color = if ui.visuals().dark_mode {
            egui::Color32::from_rgba_unmultiplied(220, 220, 220, 160)
        } else {
            egui::Color32::from_rgba_unmultiplied(90, 90, 90, 180)
        };

        let source_text = t!("floating_window.from_source", source = self.source.clone());
        ui.painter().text(
            egui::pos2(card_rect.max.x - 2.0, card_rect.min.y - 9.0),
            egui::Align2::RIGHT_CENTER,
            source_text,
            egui::FontId::proportional(9.0),
            source_text_color,
        );

        ui.painter().text(
            egui::pos2(card_rect.center().x, card_rect.center().y - 1.0),
            egui::Align2::CENTER_CENTER,
            self.formatted_code(),
            egui::FontId::monospace(22.0),
            primary_text_color,
        );

        ui.painter().text(
            egui::pos2(card_rect.center().x, card_rect.max.y + 10.0),
            egui::Align2::CENTER_CENTER,
            t!("floating_window.click_to_fill"),
            egui::FontId::proportional(10.0),
            hint_text_color,
        );

        response
    }
}

impl App for VerificationCodeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.should_close || self.created_at.elapsed() > self.lifetime {
            let ctx_clone = ctx.clone();
            std::thread::spawn(move || {
                ctx_clone.send_viewport_cmd(egui::ViewportCommand::Close);
            });
            return;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            self.handle_window_drag(ui, ctx);
            self.draw_close_button(ui, ctx);
            self.draw_content(ui, ctx);
        });
    }
}
