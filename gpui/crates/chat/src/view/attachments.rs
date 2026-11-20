//! Native macOS codecs prepare images off the UI thread. Rasterizing first
//! strips source metadata, matching Iced's canvas preparation.
use super::*;
use gitterm_core::chat::types::{ImageAttachment, MAX_IMAGES_PER_MESSAGE, validate_images};
use gpui::StyledImage;
use std::{path::PathBuf, process::Command};

pub(super) enum Source {
    Clipboard(gpui::Image),
    File(PathBuf),
}
pub(super) struct Prepared {
    payload: ImageAttachment,
    preview: Arc<gpui::Image>,
}

fn file_format(path: &std::path::Path) -> Option<gpui::ImageFormat> {
    use gpui::ImageFormat::*;
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "png" => Png,
            "jpg" | "jpeg" => Jpeg,
            "gif" => Gif,
            "webp" => Webp,
            "bmp" => Bmp,
            "tif" | "tiff" => Tiff,
            "ico" => Ico,
            "svg" => Svg,
            "pnm" | "ppm" | "pgm" | "pbm" => Pnm,
            _ => return None,
        },
    )
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gitterm-v6-gpui-image-{}-{}-{}",
            std::process::id(),
            crate::composer::now_ms(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| format!("Create image preparation folder: {e}"))?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        for name in ["input", "output"] {
            if let Err(error) = std::fs::remove_file(self.0.join(name))
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("Remove temporary chat image: {error}");
            }
        }
        if let Err(error) = std::fs::remove_dir(&self.0) {
            eprintln!("Remove temporary chat image folder: {error}");
        }
    }
}

pub(super) struct Thumbnail {
    image: Arc<gpui::Image>,
    size: gpui::Size<gpui::Pixels>,
}

fn transcript_thumbnail(
    payload: &ImageAttachment,
    renderer: gpui::SvgRenderer,
) -> Result<Thumbnail, String> {
    validate_images(std::slice::from_ref(payload))?;
    let format = gpui::ImageFormat::from_mime_type(&payload.media_type)
        .ok_or_else(|| "Unsupported sent image type".to_string())?;
    // Use the same native codec tooling as composer ingress, off the UI thread.
    // The private scratch directory prevents attachment data from being public.
    let scratch = Scratch::new()?;
    let input = scratch.0.join("input");
    std::fs::write(&input, &payload.data)
        .map_err(|error| format!("Prepare sent image: {error}"))?;
    let decoded = Command::new("/usr/bin/base64")
        .args(["-D", "-i"])
        .arg(&input)
        .output()
        .map_err(|error| format!("Decode sent image: {error}"))?;
    if !decoded.status.success() {
        return Err("Could not decode sent image base64".into());
    }
    let image = Arc::new(gpui::Image::from_bytes(format, decoded.stdout));
    let decoded = image
        .to_image_data(renderer)
        .map_err(|_| "Could not decode sent image pixels".to_string())?;
    let intrinsic = decoded.size(0);
    let width = intrinsic.width.0 as f32;
    let height = intrinsic.height.0 as f32;
    if width <= 0. || height <= 0. {
        return Err("Sent image has no pixels".into());
    }
    let scale = (160. / width).min(120. / height).min(1.);
    Ok(Thumbnail {
        image,
        size: size(px(width * scale), px(height * scale)),
    })
}

impl RowView {
    pub(super) fn prepare_sent_images(&mut self, cx: &mut Context<Self>) {
        self.preparing_sent_images = None;
        self.sent_images.clear();
        if self.row.images.is_empty() {
            return;
        }
        let images = self.row.images.clone();
        let renderer = cx.svg_renderer().clone();
        self.preparing_sent_images = Some(cx.spawn(async move |this, cx| {
            let thumbnails = cx
                .background_executor()
                .spawn(async move {
                    images
                        .iter()
                        .map(|image| transcript_thumbnail(image, renderer.clone()))
                        .collect()
                })
                .await;
            _ = this.update(cx, |row, cx| {
                row.sent_images = thumbnails;
                row.preparing_sent_images = None;
                row.scroller.update(cx, |state, cx| {
                    state.remeasure_items(row.index..row.index + 1, cx);
                });
                cx.notify();
            });
        }));
    }

    pub(super) fn render_sent_images(&self) -> impl IntoElement {
        let palette = self.appearance.palette();
        div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .items_start()
            .children(self.preparing_sent_images.is_some().then(|| {
                div()
                    .text_color(rgb(palette.muted))
                    .child("Loading attached images…")
            }))
            .children(
                self.sent_images
                    .iter()
                    .enumerate()
                    .map(|(index, thumbnail)| {
                        let selector = format!("sent-image-{}-{index}", self.row.id);
                        let container = div()
                            .id(gpui::SharedString::from(selector.clone()))
                            .debug_selector(move || selector.clone())
                            .role(gpui::Role::Image)
                            .aria_label(format!("Attached image {}", index + 1))
                            .rounded(px(6.))
                            .overflow_hidden();
                        match thumbnail {
                            Ok(thumbnail) => container
                                .child(
                                    gpui::img(thumbnail.image.clone())
                                        .w(thumbnail.size.width)
                                        .h(thumbnail.size.height)
                                        .object_fit(gpui::ObjectFit::Contain)
                                        .border_1()
                                        .border_color(rgb(palette.border))
                                        .rounded(px(6.)),
                                )
                                .into_any_element(),
                            Err(error) => container
                                .text_color(rgb(palette.err))
                                .child(error.clone())
                                .into_any_element(),
                        }
                    }),
            )
    }
}

fn prepare(source: Source, renderer: gpui::SvgRenderer) -> Result<Prepared, String> {
    let image = match source {
        Source::Clipboard(image) => image,
        Source::File(path) => {
            let format =
                file_format(&path).ok_or_else(|| "Only images can be attached.".to_string())?;
            gpui::Image::from_bytes(
                format,
                std::fs::read(&path).map_err(|e| format!("Read {}: {e}", path.display()))?,
            )
        }
    };
    let decoded = image
        .to_image_data(renderer)
        .map_err(|e| format!("Decode attached image: {e}"))?;
    let size = decoded.size(0);
    let keep_png = image.format() == gpui::ImageFormat::Png
        && decoded
            .as_bytes(0)
            .is_some_and(|bytes| bytes.as_chunks::<4>().0.iter().any(|pixel| pixel[3] < 255));
    let scratch = Scratch::new()?;
    let input = scratch.0.join("input");
    let output = scratch.0.join("output");
    // Re-encode decoded pixels rather than copying source metadata (canvas
    // does the same in Iced). TGA's raw BGRA layout matches GPUI's first frame.
    let width =
        u16::try_from(size.width.0).map_err(|_| "Image is too wide to prepare".to_string())?;
    let height =
        u16::try_from(size.height.0).map_err(|_| "Image is too tall to prepare".to_string())?;
    let pixels = decoded
        .as_bytes(0)
        .ok_or_else(|| "Image has no decoded pixels".to_string())?;
    let mut header = [0u8; 18];
    header[2] = 2; // Uncompressed true color.
    header[12..14].copy_from_slice(&width.to_le_bytes());
    header[14..16].copy_from_slice(&height.to_le_bytes());
    header[16] = 32;
    header[17] = 0x28; // Top-left origin, eight alpha bits.
    {
        use std::io::Write;
        let mut file =
            std::fs::File::create(&input).map_err(|e| format!("Prepare image pixels: {e}"))?;
        file.write_all(&header)
            .and_then(|_| file.write_all(pixels))
            .map_err(|e| format!("Write image pixels: {e}"))?;
    }
    let mut command = Command::new("/usr/bin/sips");
    command.args(["-s", "format", if keep_png { "png" } else { "jpeg" }]);
    if !keep_png {
        command.args(["-s", "formatOptions", "85"]);
    }
    if size.width.0.max(size.height.0) > 1568 {
        command.args(["-Z", "1568"]);
    }
    let result = command
        .arg(&input)
        .arg("--out")
        .arg(&output)
        .output()
        .map_err(|e| format!("Prepare attached image with macOS codecs: {e}"))?;
    if !result.status.success() {
        return Err("Could not resize or encode the attached image.".into());
    }
    let bytes = std::fs::read(&output).map_err(|e| format!("Read prepared image: {e}"))?;
    let encoded = Command::new("/usr/bin/base64")
        .arg("-i")
        .arg(&output)
        .output()
        .map_err(|e| format!("Encode attached image: {e}"))?;
    if !encoded.status.success() {
        return Err("Could not base64 encode the attached image.".into());
    }
    let payload = ImageAttachment {
        media_type: if keep_png { "image/png" } else { "image/jpeg" }.into(),
        data: String::from_utf8(encoded.stdout)
            .map_err(|e| format!("Encode attached image: {e}"))?
            .split_whitespace()
            .collect(),
    };
    validate_images(std::slice::from_ref(&payload))?;
    Ok(Prepared {
        payload,
        preview: Arc::new(gpui::Image::from_bytes(
            if keep_png {
                gpui::ImageFormat::Png
            } else {
                gpui::ImageFormat::Jpeg
            },
            bytes,
        )),
    })
}

impl ChatView {
    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.read(cx).focus_handle(cx).focus(window, cx);
    }
    pub(crate) fn render_attachments(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("composer-attachments")
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .children(self.images.iter().enumerate().map(|(index, _)| {
                let preview = self.image_previews.get(index).cloned().flatten();
                div()
                    .id(("attached-image", index))
                    .relative()
                    .size(px(56.))
                    .border_1()
                    .border_color(rgb(self.appearance.palette().strong))
                    .overflow_hidden()
                    .rounded(px(6.))
                    .bg(rgb(self.appearance.palette().alt))
                    .role(gpui::Role::Group)
                    .aria_label(format!("Attached image {}", index + 1))
                    .children(preview.map(|image| {
                        gpui::img(image)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Cover)
                            .rounded(px(6.))
                    }))
                    .child(
                        Button::new(("remove-image", index))
                            .debug_selector(move || format!("remove-image-{index}"))
                            .label("×")
                            .tooltip("Remove image")
                            .accessibility_label("Remove image")
                            .size(px(18.))
                            .p_0()
                            .rounded_full()
                            .bg(gpui::rgba(0x11111bcc))
                            .absolute()
                            .top(px(2.))
                            .right(px(2.))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.remove_image(index, window, cx)
                            })),
                    )
            }))
    }
    pub(crate) fn paste_images(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let sources: Vec<_> = item
            .entries()
            .iter()
            .flat_map(|entry| match entry {
                gpui::ClipboardEntry::Image(image) => vec![Source::Clipboard(image.clone())],
                gpui::ClipboardEntry::ExternalPaths(paths) => paths
                    .0
                    .iter()
                    .filter(|path| file_format(path).is_some())
                    .cloned()
                    .map(Source::File)
                    .collect(),
                _ => vec![],
            })
            .collect();
        if sources.is_empty() {
            return false;
        }
        self.enqueue_images(sources, cx);
        true
    }
    pub(crate) fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let sources: Vec<_> = paths
            .into_iter()
            .filter(|path| file_format(path).is_some())
            .map(Source::File)
            .collect();
        if sources.is_empty() {
            self.set_status_error("Only images can be attached.".into(), cx);
        }
        self.enqueue_images(sources, cx);
    }
    fn enqueue_images(&mut self, sources: Vec<Source>, cx: &mut Context<Self>) {
        if self.read_only_reason.is_some() || self.callback.is_none() {
            return;
        }
        self.image_queue.extend(sources);
        self.prepare_next_image(cx);
    }
    fn prepare_next_image(&mut self, cx: &mut Context<Self>) {
        if self.preparing_image.is_some() {
            return;
        }
        let Some(source) = self.image_queue.pop_front() else {
            return;
        };
        if self.images.len() >= MAX_IMAGES_PER_MESSAGE {
            self.image_queue.clear();
            self.set_status_error(
                "A message can carry at most 4 images; extra images were not attached.".into(),
                cx,
            );
            return;
        }
        let renderer = cx.svg_renderer();
        self.preparing_image = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { prepare(source, renderer) })
                .await;
            _ = this.update(cx, |this, cx| {
                this.preparing_image = None;
                if this.read_only_reason.is_none() {
                    match result {
                        Ok(prepared) => {
                            let mut images = this.images.clone();
                            images.push(prepared.payload);
                            match validate_images(&images) {
                                Ok(()) => {
                                    this.images = images;
                                    this.image_previews.push(Some(prepared.preview));
                                }
                                Err(error) => this.set_status_error(error, cx),
                            }
                        }
                        Err(error) => this.set_status_error(error, cx),
                    }
                }
                this.prepare_next_image(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }
    pub(crate) fn remove_image(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index < self.images.len() {
            self.images.remove(index);
            self.image_previews.remove(index);
            self.composer
                .update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }
    pub(crate) fn pick_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only_reason.is_some() || self.callback.is_none() {
            return;
        }
        let response = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach images".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let response = response.await;
            _ = this.update_in(cx, |this, window, cx| {
                match response {
                    Ok(Ok(Some(paths))) => this.attach_paths(paths, cx),
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => this.set_status_error(format!("Choose images: {error}"), cx),
                    Err(error) => this.set_status_error(format!("Choose images: {error}"), cx),
                }
                this.composer
                    .update(cx, |input, cx| input.focus(window, cx));
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PNG: &[u8] = include_bytes!("../../../../../assets/icon.iconset/icon_32x32.png");

    #[gpui::test]
    fn native_preparation_preserves_transparency_and_reports_bad_images(
        cx: &mut gpui::TestAppContext,
    ) {
        let renderer = cx.update(|cx| cx.svg_renderer());
        let prepared = prepare(
            Source::Clipboard(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                PNG.to_vec(),
            )),
            renderer.clone(),
        )
        .unwrap();
        let original = gpui::Image::from_bytes(gpui::ImageFormat::Png, PNG.to_vec())
            .to_image_data(renderer.clone())
            .unwrap();
        let thumbnail = transcript_thumbnail(&prepared.payload, renderer.clone()).unwrap();
        assert_eq!(
            thumbnail.size,
            size(px(32.), px(32.)),
            "small sent images are not enlarged"
        );
        let mut invalid = prepared.payload.clone();
        invalid.data = "not-base64!".into();
        assert!(transcript_thumbnail(&invalid, renderer.clone()).is_err());
        let roundtrip = prepared.preview.to_image_data(renderer.clone()).unwrap();
        assert_eq!(
            original.as_bytes(0),
            roundtrip.as_bytes(0),
            "PNG raster orientation, channels, and transparency are preserved"
        );
        assert_eq!(prepared.payload.media_type, "image/png");
        assert!(prepared.payload.data.starts_with("iVBOR"));
        assert!(
            prepared
                .payload
                .data
                .bytes()
                .all(|byte| !byte.is_ascii_whitespace())
        );
        assert!(prepare(Source::Clipboard(gpui::Image::empty()), renderer).is_err());
    }

    #[gpui::test]
    fn native_preparation_downscales_to_iced_longest_edge(cx: &mut gpui::TestAppContext) {
        let scratch = Scratch::new().unwrap();
        let input = scratch.0.join("input");
        let output = scratch.0.join("output");
        std::fs::write(&input, PNG).unwrap();
        let status = Command::new("/usr/bin/sips")
            .args(["-z", "1800", "2000"])
            .arg(&input)
            .arg("--out")
            .arg(&output)
            .output()
            .unwrap();
        assert!(status.status.success());
        let source = Source::Clipboard(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            std::fs::read(output).unwrap(),
        ));
        let renderer = cx.update(|cx| cx.svg_renderer());
        let prepared = prepare(source, renderer.clone()).unwrap();
        let thumbnail = transcript_thumbnail(&prepared.payload, renderer.clone()).unwrap();
        assert_eq!(thumbnail.size.height, px(120.));
        assert!(thumbnail.size.width < px(160.));
        let size = prepared.preview.to_image_data(renderer).unwrap().size(0);
        assert_eq!(size.width.0, 1568);
        assert!(size.height.0 < 1568);
    }
}
