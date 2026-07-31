use super::*;

impl App {
    pub(super) fn handle_paste(&mut self, text: String) {
        if let Some(path) = single_image_path(&text) {
            match self.attach_image_path(&path) {
                Ok(()) => return,
                Err(error) => {
                    self.status_line = format!("image attach failed: {error}");
                    self.toast("Image attach failed", ToastKind::Error);
                    return;
                }
            }
        }

        let index = self.input_byte_index(self.input_cursor);
        let pasted_chars = text.chars().count();
        self.input.insert_str(index, &text);
        self.input_cursor += pasted_chars;
        self.clamp_slash_selection();
        self.refresh_mention_state();
    }

    pub(super) fn paste_image_from_clipboard(&mut self) {
        match self.read_clipboard_image() {
            Ok(attachment) => {
                let label = attachment_label(&attachment);
                self.cache_attachment_preview(&attachment);
                self.pending_attachments.push(attachment);
                self.status_line = format!("attached {label}");
                self.toast("Image attached", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("clipboard image unavailable: {error}");
                self.toast("No clipboard image", ToastKind::Warning);
            }
        }
    }

    pub(super) fn read_clipboard_image(&self) -> Result<ImageAttachment> {
        let mut clipboard = Clipboard::new().wrap_err("failed to open clipboard")?;
        let image = clipboard
            .get_image()
            .wrap_err("clipboard does not contain an image")?;
        let width = image.width as u32;
        let height = image.height as u32;
        let rgba = image.bytes.into_owned();
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&rgba, width, height, ColorType::Rgba8.into())
            .wrap_err("failed to encode clipboard image")?;
        self.store_image_bytes("clipboard", "image/png", width, height, png)
    }

    pub(super) fn attach_image_path(&mut self, path: &Path) -> Result<()> {
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let image = image::load_from_memory(&bytes)
            .wrap_err_with(|| format!("failed to decode image {}", path.display()))?;
        let mime = image_mime_from_path(path).unwrap_or("image/png");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image");
        let attachment =
            self.store_image_bytes(name, mime, image.width(), image.height(), bytes)?;
        let label = attachment_label(&attachment);
        self.cache_attachment_preview(&attachment);
        self.pending_attachments.push(attachment);
        self.status_line = format!("attached {label}");
        self.toast("Image attached", ToastKind::Success);
        Ok(())
    }

    pub(super) fn cache_attachment_preview(&mut self, attachment: &ImageAttachment) {
        self.attachment_previews.insert(
            attachment.id.clone(),
            image_preview_lines(attachment, COMPOSER_IMAGE_PREVIEW_WIDTH),
        );
    }

    pub(super) fn store_image_bytes(
        &self,
        name_hint: &str,
        mime: &str,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    ) -> Result<ImageAttachment> {
        let id = format!("image-{}", attachment_timestamp());
        let extension = image_extension(mime);
        let safe_name = sanitize_attachment_name(name_hint, extension);
        let file_name = format!("{id}-{safe_name}");
        let dir = self.attachment_dir()?;
        let path = dir.join(file_name);
        atomic_write_private(&path, &bytes)
            .wrap_err_with(|| format!("failed to write {}", path.display()))?;
        Ok(ImageAttachment {
            id,
            name: safe_name,
            path,
            mime: mime.to_string(),
            width,
            height,
            size_bytes: bytes.len() as u64,
        })
    }

    pub(super) fn attachment_dir(&self) -> Result<PathBuf> {
        if let Some(session) = &self.session {
            return Ok(session.attachment_dir());
        }
        Ok(env::current_dir()?.join(".medusa").join("attachments"))
    }

    pub(super) fn image_attachments(&self) -> Vec<ImageAttachment> {
        let mut attachments = Vec::new();
        for item in &self.transcript {
            if let TranscriptItem::Message(message) = item {
                attachments.extend(message.attachments.iter().cloned());
            }
        }
        attachments.extend(self.pending_attachments.iter().cloned());
        attachments
    }

    pub(super) fn current_preview_image(&self) -> Option<ImageAttachment> {
        let attachments = self.image_attachments();
        if attachments.is_empty() {
            return None;
        }
        attachments
            .get(
                self.image_preview_index
                    .min(attachments.len().saturating_sub(1)),
            )
            .cloned()
    }

    pub(super) fn current_preview_image_is_pending(&self) -> bool {
        self.current_preview_image().is_some_and(|attachment| {
            self.pending_attachments
                .iter()
                .any(|pending| pending.id == attachment.id)
        })
    }

    pub(super) fn open_image_preview(&mut self, index: usize) {
        let count = self.image_attachments().len();
        if count == 0 {
            self.status_line = "no images to preview".to_string();
            self.toast("No images attached", ToastKind::Info);
            return;
        }

        self.image_preview_index = index.min(count.saturating_sub(1));
        self.image_preview_zoom = self
            .image_preview_zoom
            .clamp(IMAGE_PREVIEW_MIN_ZOOM, IMAGE_PREVIEW_MAX_ZOOM);
        self.active_modal = Some(Modal::ImagePreview);
        self.status_line = format!("image preview {}/{}", self.image_preview_index + 1, count);
    }

    pub(super) fn open_latest_image_preview(&mut self) {
        let count = self.image_attachments().len();
        if count == 0 {
            self.status_line = "no images to preview".to_string();
            self.toast("No images attached", ToastKind::Info);
            return;
        }
        self.open_image_preview(count.saturating_sub(1));
    }

    pub(super) fn open_image_preview_for_attachment(&mut self, attachment: &ImageAttachment) {
        let attachments = self.image_attachments();
        let index = attachments
            .iter()
            .position(|candidate| candidate.id == attachment.id)
            .unwrap_or_else(|| attachments.len().saturating_sub(1));
        self.open_image_preview(index);
    }

    pub(super) fn move_image_preview_next(&mut self) {
        let count = self.image_attachments().len();
        if count == 0 {
            return;
        }
        self.image_preview_index = (self.image_preview_index + 1) % count;
        self.status_line = format!("image preview {}/{}", self.image_preview_index + 1, count);
    }

    pub(super) fn move_image_preview_previous(&mut self) {
        let count = self.image_attachments().len();
        if count == 0 {
            return;
        }
        self.image_preview_index = if self.image_preview_index == 0 {
            count - 1
        } else {
            self.image_preview_index - 1
        };
        self.status_line = format!("image preview {}/{}", self.image_preview_index + 1, count);
    }

    pub(super) fn move_image_preview_first(&mut self) {
        if !self.image_attachments().is_empty() {
            self.image_preview_index = 0;
            self.status_line = "first image".to_string();
        }
    }

    pub(super) fn move_image_preview_last(&mut self) {
        let count = self.image_attachments().len();
        if count > 0 {
            self.image_preview_index = count - 1;
            self.status_line = "last image".to_string();
        }
    }

    pub(super) fn zoom_image_preview_in(&mut self) {
        self.image_preview_zoom = self
            .image_preview_zoom
            .saturating_add(IMAGE_PREVIEW_ZOOM_STEP)
            .min(IMAGE_PREVIEW_MAX_ZOOM);
        self.status_line = format!("image zoom {}%", self.image_preview_zoom);
    }

    pub(super) fn zoom_image_preview_out(&mut self) {
        self.image_preview_zoom = self
            .image_preview_zoom
            .saturating_sub(IMAGE_PREVIEW_ZOOM_STEP)
            .max(IMAGE_PREVIEW_MIN_ZOOM);
        self.status_line = format!("image zoom {}%", self.image_preview_zoom);
    }

    pub(super) fn reset_image_preview_zoom(&mut self) {
        self.image_preview_zoom = 100;
        self.status_line = "image zoom reset".to_string();
    }

    pub(super) fn open_selected_preview_image_external(&mut self) {
        let Some(attachment) = self.current_preview_image() else {
            self.status_line = "no image selected".to_string();
            return;
        };

        let result = if cfg!(target_os = "macos") {
            Command::new("open").arg(&attachment.path).spawn()
        } else if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(&attachment.path)
                .spawn()
        } else {
            Command::new("xdg-open").arg(&attachment.path).spawn()
        };

        match result {
            Ok(_) => {
                self.status_line = format!("opened {}", attachment.name);
                self.toast("Image opened", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("open image failed: {error}");
                self.toast("Open image failed", ToastKind::Error);
            }
        }
    }

    pub(super) fn copy_selected_preview_image_path(&mut self) {
        let Some(attachment) = self.current_preview_image() else {
            self.status_line = "no image selected".to_string();
            return;
        };

        let path = attachment.path.to_string_lossy().to_string();
        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(path.clone())) {
            Ok(()) => {
                self.status_line = "image path copied".to_string();
                self.toast("Image path copied", ToastKind::Success);
            }
            Err(error) => {
                self.status_line = format!("copy path failed: {error}");
                self.toast("Copy path failed", ToastKind::Error);
            }
        }
    }

    pub(super) fn detach_latest_pending_attachment(&mut self) {
        let Some(index) = self.pending_attachments.len().checked_sub(1) else {
            self.status_line = "no pending image to detach".to_string();
            self.toast("No pending image", ToastKind::Info);
            return;
        };
        self.detach_pending_attachment_at(index);
        self.status_line = "detached latest image".to_string();
        self.toast("Image detached", ToastKind::Success);
    }

    pub(super) fn detach_current_preview_image(&mut self) {
        let Some(attachment) = self.current_preview_image() else {
            self.status_line = "no image selected".to_string();
            return;
        };
        let Some(index) = self
            .pending_attachments
            .iter()
            .position(|pending| pending.id == attachment.id)
        else {
            self.status_line = "sent image stays in transcript".to_string();
            self.toast("Only pending images can be detached", ToastKind::Warning);
            return;
        };

        self.detach_pending_attachment_at(index);
        let remaining = self.image_attachments().len();
        if remaining == 0 {
            self.active_modal = None;
            self.image_preview_index = 0;
            self.status_line = "image detached".to_string();
        } else {
            self.image_preview_index = self.image_preview_index.min(remaining.saturating_sub(1));
            self.status_line = format!(
                "image detached · preview {}/{}",
                self.image_preview_index + 1,
                remaining
            );
        }
        self.toast("Image detached", ToastKind::Success);
    }

    pub(super) fn detach_pending_attachment_at(&mut self, index: usize) -> Option<ImageAttachment> {
        if index >= self.pending_attachments.len() {
            return None;
        }
        let attachment = self.pending_attachments.remove(index);
        self.attachment_previews.remove(&attachment.id);
        self.image_renderer.forget(&attachment.id);
        let _ = fs::remove_file(&attachment.path);
        Some(attachment)
    }
}
