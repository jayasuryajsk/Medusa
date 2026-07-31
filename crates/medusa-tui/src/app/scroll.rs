use super::*;

impl App {
    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.active_modal == Some(Modal::ImagePreview) {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.move_image_preview_previous(),
                MouseEventKind::ScrollDown => self.move_image_preview_next(),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.status_line = "image preview".to_string()
                }
                _ => {}
            }
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(attachment) = self.image_attachment_at_mouse(mouse) {
                    self.open_image_preview_for_attachment(&attachment);
                }
            }
            MouseEventKind::ScrollUp => self.scroll_chat_up(self.mouse_scroll_amount(mouse)),
            MouseEventKind::ScrollDown => self.scroll_chat_down(self.mouse_scroll_amount(mouse)),
            _ => {}
        }
    }

    pub(super) fn mouse_scroll_amount(&self, mouse: MouseEvent) -> usize {
        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
            self.chat_page_scroll_amount()
        } else if mouse.modifiers.contains(KeyModifiers::CONTROL) {
            1
        } else {
            6
        }
    }

    pub(super) fn image_attachment_at_mouse(&self, mouse: MouseEvent) -> Option<ImageAttachment> {
        let area = self.last_chat_viewport?;
        let rows;
        let rows_ref = if self.last_transcript_rows.is_empty() {
            rows = self.visible_transcript_rows();
            &rows
        } else {
            &self.last_transcript_rows
        };
        if rows_ref.is_empty() {
            return None;
        }

        let metrics = chat_viewport_metrics(rows_ref, area, self.chat_scroll);
        let text_area = metrics.text_area;
        let text_right = text_area.x.saturating_add(text_area.width);
        let text_bottom = text_area.y.saturating_add(text_area.height);
        if mouse.column < text_area.x
            || mouse.column >= text_right
            || mouse.row < text_area.y
            || mouse.row >= text_bottom
        {
            return None;
        }

        for placement in transcript_image_placements(rows_ref, text_area, metrics.top_offset) {
            let x0 = text_area.x.saturating_add(placement.x_offset);
            let x1 = x0.saturating_add(placement.width).min(text_right);
            let y0_raw = text_area.y as i32 + placement.y_offset as i32;
            let y1_raw = y0_raw + placement.height as i32;
            let y0 = y0_raw.max(text_area.y as i32);
            let y1 = y1_raw.min(text_bottom as i32);

            if x0 < x1
                && y0 < y1
                && mouse.column >= x0
                && mouse.column < x1
                && (mouse.row as i32) >= y0
                && (mouse.row as i32) < y1
            {
                return Some(placement.attachment);
            }
        }

        None
    }

    pub(super) fn scroll_chat_up(&mut self, amount: usize) {
        self.chat_scroll = self.chat_scroll.saturating_add(amount);
        self.chat_scroll_target = self.chat_scroll;
        self.clamp_chat_scroll_to_viewport();
        self.status_line = self.scroll_status_text();
    }

    pub(super) fn scroll_chat_down(&mut self, amount: usize) {
        self.chat_scroll = self.chat_scroll.saturating_sub(amount);
        self.chat_scroll_target = self.chat_scroll;
        self.clamp_chat_scroll_to_viewport();
        self.status_line = self.scroll_status_text();
    }

    pub(super) fn scroll_chat_to_top(&mut self) {
        self.chat_scroll = self
            .current_chat_viewport_metrics_for_scroll(self.chat_scroll_target)
            .map_or(usize::MAX / 2, |metrics| metrics.max_scroll);
        self.chat_scroll_target = self.chat_scroll;
        self.status_line = "top".to_string();
    }

    pub(super) fn scroll_chat_to_bottom(&mut self) {
        self.chat_scroll = 0;
        self.chat_scroll_target = 0;
    }

    pub(super) fn scroll_status_text(&self) -> String {
        let Some(metrics) = self.current_chat_viewport_metrics_for_scroll(self.chat_scroll_target)
        else {
            return "bottom".to_string();
        };
        if metrics.max_scroll == 0 || self.chat_scroll_target == 0 {
            return "bottom".to_string();
        }
        if self.chat_scroll_target >= metrics.max_scroll {
            return "top".to_string();
        }

        let progress = scroll_progress_percent(&metrics);
        format!("scroll {progress}% · ctrl+end bottom")
    }

    pub(super) fn clamp_chat_scroll_to_viewport(&mut self) {
        if let Some(metrics) =
            self.current_chat_viewport_metrics_for_scroll(self.chat_scroll_target)
        {
            self.chat_scroll_target = self.chat_scroll_target.min(metrics.max_scroll);
            self.chat_scroll = self.chat_scroll.min(metrics.max_scroll);
        }
    }

    pub(super) fn chat_page_scroll_amount(&self) -> usize {
        self.last_chat_viewport
            .map(|area| area.height.saturating_sub(2).max(1) as usize)
            .unwrap_or(12)
    }

    pub(super) fn current_chat_viewport_metrics(&self) -> Option<ChatViewportMetrics> {
        self.current_chat_viewport_metrics_for_scroll(self.chat_scroll)
    }

    pub(super) fn current_chat_viewport_metrics_for_scroll(
        &self,
        scroll: usize,
    ) -> Option<ChatViewportMetrics> {
        let area = self.last_chat_viewport?;
        if self.last_transcript_rows.is_empty() {
            let rows = self.visible_transcript_rows();
            return Some(chat_viewport_metrics(&rows, area, scroll));
        }
        Some(chat_viewport_metrics(
            &self.last_transcript_rows,
            area,
            scroll,
        ))
    }

    pub(super) fn stick_chat_to_bottom_if_needed(&mut self) {
        if self.chat_scroll == 0 && self.chat_scroll_target == 0 {
            self.scroll_chat_to_bottom();
        } else {
            self.clamp_chat_scroll_to_viewport();
        }
    }
}
