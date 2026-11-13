//! Images the administrator attaches to a message, from the uploaded files: shown to a model that sees images, and
//! read by OCR for the message's text alone.
//!
//! Conversations store text only: an image's place in its message is a line naming it, followed by the text read
//! from it. The image itself is kept in memory, for the model, while Kelpy runs (see the turns' `Seen`).

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use kelpy_providers::Image;

use crate::files::is_valid_name;

/// The most images one message may show.
pub const MAX_IMAGES: usize = 8;
/// The longest side of an image shown to a model: what Anthropic's models take without scaling it down, and enough
/// for a screenshot's text.
const LONGEST_SIDE: u32 = 1568;
/// The most an image shown to a model may take, encoded: under the 5 MB the strictest service accepts.
const MAX_ENCODED: usize = 3_500_000;
/// The largest file taken as an image, and the most pixels decoded from it.
const MAX_FILE: u64 = 64 << 20;
const MAX_PIXELS: u64 = 60_000_000;
/// The most of the text read from one image kept in its message.
const MAX_READ: usize = 16 << 10;
/// How long reading an image's text may take.
const READ_TIME: Duration = Duration::from_secs(60);

/// The images prepared or read at once, by every conversation: each worker holds a permit until it has ended,
/// even when its message no longer waits for it, so they never pile up.
static WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// An image attached to a message.
#[derive(Debug)]
pub struct Attached {
    /// Its name among the uploaded files.
    pub name: String,
    pub path: PathBuf,
    /// The image for a model that sees images, with the tokens it takes, or why it cannot be shown.
    pub shown: Result<(Arc<Image>, u64), String>,
    /// The text read from it by OCR, or why it could not be read.
    pub read: Result<String, String>,
}

impl Attached {
    /// The line naming the image in its message.
    fn line(&self) -> String {
        format!(
            "[Image: {}, uploaded to `{}`]",
            self.name,
            self.path.display()
        )
    }
}

/// Attaches the uploaded files `names`, in `uploads`, each prepared for the model and read by OCR. Refuses a name
/// that is not an uploaded file, a file that is not an image, and more than [`MAX_IMAGES`]. Dropped, it stops the
/// reading under way; an image being prepared, which takes a second at most, is finished first.
pub async fn attach(uploads: &Path, names: &[String]) -> Result<Vec<Attached>, String> {
    if names.len() > MAX_IMAGES {
        return Err(format!("a message can show at most {MAX_IMAGES} images"));
    }
    let mut attached = Vec::with_capacity(names.len());
    for name in names {
        if !is_valid_name(name) {
            return Err(format!("{name:?} is not the name of an uploaded file"));
        }
        let path = uploads.join(name);
        let bytes = read_image(&path)
            .await
            .map_err(|problem| format!("{name}: {problem}"))?;
        let permit = worker().await;
        let shown = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            for_model(&bytes)
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        let read = match tokio::time::timeout(READ_TIME, read_text(&path)).await {
            Ok(read) => read.map(|text| cut(text.trim(), MAX_READ)),
            Err(_) => Err(format!(
                "reading it took longer than {} s",
                READ_TIME.as_secs()
            )),
        };
        attached.push(Attached {
            name: name.clone(),
            path,
            shown: shown.map(|(image, tokens)| (Arc::new(image), tokens)),
            read,
        });
    }
    Ok(attached)
}

/// A permit to prepare or read an image, once fewer than the most are at work.
async fn worker() -> tokio::sync::SemaphorePermit<'static> {
    // Never closed.
    WORKERS
        .acquire()
        .await
        .expect("the workers' semaphore is open")
}

/// The content of the image at `path`, refused when it is not a file in a format Kelpy reads or is too large.
async fn read_image(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| "there is no such uploaded file".to_owned())?;
    if !metadata.is_file() {
        return Err("it is not a file".to_owned());
    }
    if metadata.len() > MAX_FILE {
        return Err(format!("an image may take at most {} MiB", MAX_FILE >> 20));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("cannot read it: {error}"))?;
    match image::guess_format(&bytes) {
        Ok(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP) => {
            Ok(bytes)
        }
        _ => Err("it is not a PNG, JPEG, GIF or WebP image".to_owned()),
    }
}

/// The image as a model takes it, with the tokens it takes: as it is when it is a PNG or a JPEG small enough, else
/// scaled down to [`LONGEST_SIDE`] and encoded again, as PNG for a screenshot's sharp text or JPEG when that is too
/// large.
fn for_model(bytes: &[u8]) -> Result<(Image, u64), String> {
    let reader = || -> Result<ImageReader<Cursor<&[u8]>>, String> {
        let mut reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|error| error.to_string())?;
        let mut limits = Limits::default();
        limits.max_alloc = Some(MAX_PIXELS * 4);
        reader.limits(limits);
        Ok(reader)
    };
    let format = image::guess_format(bytes).map_err(|error| error.to_string())?;
    let (width, height) = reader()?
        .into_dimensions()
        .map_err(|error| format!("cannot read it as an image: {error}"))?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!("{width}×{height} pixels are more than Kelpy reads"));
    }
    let fits = width.max(height) <= LONGEST_SIDE && bytes.len() <= MAX_ENCODED;
    let (data, media_type, width, height) = match format {
        ImageFormat::Png if fits => (bytes.to_vec(), "image/png", width, height),
        ImageFormat::Jpeg if fits => (bytes.to_vec(), "image/jpeg", width, height),
        _ => {
            let decoded = reader()?
                .decode()
                .map_err(|error| format!("cannot read it as an image: {error}"))?;
            let scaled = if decoded.width().max(decoded.height()) > LONGEST_SIDE {
                decoded.resize(
                    LONGEST_SIDE,
                    LONGEST_SIDE,
                    image::imageops::FilterType::Lanczos3,
                )
            } else {
                decoded
            };
            let (data, media_type) = encode(&scaled, format == ImageFormat::Jpeg)?;
            (data, media_type, scaled.width(), scaled.height())
        }
    };
    let image = Image {
        media_type,
        data: base64::engine::general_purpose::STANDARD.encode(data),
    };
    Ok((image, tokens(width, height)))
}

/// `image` encoded: as JPEG for a photograph, `photo`, or a PNG too large, else as PNG.
fn encode(image: &DynamicImage, photo: bool) -> Result<(Vec<u8>, &'static str), String> {
    let failed = |error: image::ImageError| format!("cannot prepare it for the model: {error}");
    if !photo {
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .map_err(failed)?;
        if png.len() <= MAX_ENCODED {
            return Ok((png, "image/png"));
        }
    }
    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85);
    DynamicImage::ImageRgb8(image.to_rgb8())
        .write_with_encoder(encoder)
        .map_err(failed)?;
    Ok((jpeg, "image/jpeg"))
}

/// The tokens an image of `width` by `height` pixels takes, as Anthropic counts them; OpenAI's count is close.
fn tokens(width: u32, height: u32) -> u64 {
    (u64::from(width) * u64::from(height) / 750).max(85)
}

/// `text`, cut at a character boundary to at most `limit` bytes, saying so.
fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let end = (0..=limit)
        .rev()
        .find(|&end| text.is_char_boundary(end))
        .unwrap_or(0);
    format!("{}\n[…]", &text[..end])
}

/// The message as stored, its text alone: what the administrator wrote, then each image's line and the text read
/// from it.
pub fn text_alone(text: &str, attached: &[Attached]) -> String {
    write(text, attached, false)
}

/// The message as a model that sees images reads it, with the images shown: what the administrator wrote, then
/// each image's line, and for an image that could not be shown why, and the text read from it in its place.
pub fn text_seen(text: &str, attached: &[Attached]) -> String {
    write(text, attached, true)
}

/// `text`, then each image's line followed by the text read from it, unless the image is `seen` with it.
fn write(text: &str, attached: &[Attached], seen: bool) -> String {
    let mut message = text.trim().to_owned();
    for image in attached {
        if !message.is_empty() {
            message.push_str("\n\n");
        }
        message.push_str(&image.line());
        if seen {
            match &image.shown {
                Ok(_) => continue,
                Err(problem) => message.push_str(&format!(
                    "\nIt cannot be shown to you: {}.",
                    one_line(problem)
                )),
            }
        }
        message.push('\n');
        match &image.read {
            Ok(read) if read.is_empty() => message.push_str("No text read from it by OCR."),
            Ok(read) => {
                let fence = "`".repeat(longest_run(read, '`').max(2) + 1);
                message.push_str(&format!(
                    "Text read from it by OCR:\n{fence}text\n{read}\n{fence}"
                ));
            }
            // On one line, as interfaces read it.
            Err(problem) => message.push_str(&format!(
                "Its text could not be read: {}.",
                one_line(problem)
            )),
        }
    }
    message
}

/// `problem` on one line, without a full stop at its end.
fn one_line(problem: &str) -> String {
    let problem = problem.split_whitespace().collect::<Vec<_>>().join(" ");
    problem.trim_end_matches('.').to_owned()
}

/// The longest run of `character` in `text`.
fn longest_run(text: &str, character: char) -> usize {
    text.split(|other| other != character)
        .map(str::len)
        .max()
        .unwrap_or(0)
}

/// The text in the image at `path`, by OCR: Windows' own, offline, in the languages of the user's profile;
/// elsewhere Tesseract, when installed, in the languages it has. Dropped, it stops the reading: Windows' is
/// cancelled, Tesseract is ended.
pub async fn read_text(path: &Path) -> Result<String, String> {
    let permit = worker().await;
    #[cfg(windows)]
    {
        let path = path.to_path_buf();
        let (signal, signals) = std::sync::mpsc::channel();
        // Tells the reader to stop once this future is dropped, unless it has ended already.
        let stop = windows_ocr::StopOnDrop(signal.clone());
        let read = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            windows_ocr::read(&path, &signal, &signals)
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        drop(stop);
        read
    }
    #[cfg(not(windows))]
    {
        let _permit = permit;
        tesseract(path).await
    }
}

#[cfg(windows)]
mod windows_ocr {
    use std::path::Path;
    use std::sync::mpsc::{Receiver, Sender};

    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat,
        BitmapTransform, ColorManagementMode, ExifOrientationMode,
    };
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::{FileAccessMode, StorageFile};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::{HSTRING, RuntimeType};
    use windows_future::{AsyncOperationCompletedHandler, IAsyncOperation};

    /// What the reader waits for: the step under way done, or the reading no longer wanted.
    pub(super) enum Signal {
        Done,
        Stop,
    }

    /// Sends [`Signal::Stop`] when dropped.
    pub(super) struct StopOnDrop(pub(super) Sender<Signal>);

    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            // The reader may have ended already: nobody listens then.
            let _ = self.0.send(Signal::Stop);
        }
    }

    /// The result of the step `operation`, once done; cancelled, and an error, once `signals` says stop.
    pub(super) fn finish<T: RuntimeType + 'static>(
        operation: windows::core::Result<IAsyncOperation<T>>,
        signal: &Sender<Signal>,
        signals: &Receiver<Signal>,
    ) -> Result<T, String> {
        let operation = operation.map_err(|error| unreadable(&error))?;
        let done = signal.clone();
        operation
            .SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
                let _ = done.send(Signal::Done);
                Ok(())
            }))
            .map_err(|error| unreadable(&error))?;
        // One step at a time: a Done is always this step's.
        match signals.recv() {
            Ok(Signal::Done) => operation.GetResults().map_err(|error| unreadable(&error)),
            Ok(Signal::Stop) | Err(_) => {
                let _ = operation.Cancel();
                Err("the reading was stopped".to_owned())
            }
        }
    }

    fn unreadable(error: &windows::core::Error) -> String {
        format!("Windows could not read it: {}", error.message())
    }

    /// The text in the image at `path`, one line of it a line. Blocks, each step until `signals` says it is done or
    /// the reading is to stop; `signal` is where the steps say they are done.
    pub(super) fn read(
        path: &Path,
        signal: &Sender<Signal>,
        signals: &Receiver<Signal>,
    ) -> Result<String, String> {
        // SAFETY: initialises the Windows Runtime on this thread, as its calls need; done already is just as good.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
            "Windows has no text recognition for the languages of this account: add one in Settings, Time & \
             language"
                .to_owned()
        })?;
        let failed = |error: windows::core::Error| unreadable(&error);
        let path = std::path::absolute(path).map_err(|error| error.to_string())?;
        let file = finish(
            StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str())),
            signal,
            signals,
        )?;
        let stream = finish(file.OpenAsync(FileAccessMode::Read), signal, signals)?;
        let decoder = finish(BitmapDecoder::CreateAsync(&stream), signal, signals)?;
        let (width, height) = (
            decoder.PixelWidth().map_err(failed)?,
            decoder.PixelHeight().map_err(failed)?,
        );
        // Larger images are scaled down to what the recogniser takes; smaller ones up to twice their size, as it reads
        // a screenshot's small text far better so.
        let largest = OcrEngine::MaxImageDimension().map_err(failed)?;
        let transform = BitmapTransform::new().map_err(failed)?;
        transform
            .SetInterpolationMode(BitmapInterpolationMode::Fant)
            .map_err(failed)?;
        let longest = width.max(height);
        let scale = if longest > largest {
            f64::from(largest) / f64::from(longest)
        } else {
            (f64::from(largest) / f64::from(longest)).min(2.0)
        };
        if scale != 1.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let scaled = |side: u32| ((f64::from(side) * scale) as u32).max(1);
            transform.SetScaledWidth(scaled(width)).map_err(failed)?;
            transform.SetScaledHeight(scaled(height)).map_err(failed)?;
        }
        let bitmap = finish(
            decoder.GetSoftwareBitmapTransformedAsync(
                BitmapPixelFormat::Bgra8,
                BitmapAlphaMode::Premultiplied,
                &transform,
                ExifOrientationMode::RespectExifOrientation,
                ColorManagementMode::DoNotColorManage,
            ),
            signal,
            signals,
        )?;
        let result = finish(engine.RecognizeAsync(&bitmap), signal, signals)?;
        let lines = result.Lines().map_err(failed)?;
        let mut text = Vec::new();
        for index in 0..lines.Size().map_err(failed)? {
            let line = lines
                .GetAt(index)
                .and_then(|line| line.Text())
                .map_err(failed)?;
            text.push(line.to_string_lossy());
        }
        Ok(text.join("\n"))
    }
}

/// The text in the image at `path`, read by Tesseract in every language it has but orientation detection.
#[cfg(not(windows))]
async fn tesseract(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::process::Command;

    // Found first: started by name, a folder of the PATH that cannot be searched fails as "permission denied".
    let tesseract = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|folder| folder.join("tesseract"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|found| found.is_file() && found.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| {
            let install = if cfg!(target_os = "macos") {
                "brew install tesseract"
            } else {
                "install the tesseract-ocr package"
            };
            format!("Tesseract, which reads text in images, is not installed: {install}")
        })?;
    let missing = |error: std::io::Error| format!("Tesseract could not run: {error}");
    let listed = Command::new(&tesseract)
        .arg("--list-langs")
        .kill_on_drop(true)
        .output()
        .await
        .map_err(missing)?;
    let languages: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|language| !language.is_empty() && !["osd", "equ"].contains(language))
        .map(str::to_owned)
        .collect();
    let mut command = Command::new(&tesseract);
    command.arg(path).arg("stdout").kill_on_drop(true);
    if !languages.is_empty() {
        command.arg("-l").arg(languages.join("+"));
    }
    let read = command.output().await.map_err(missing)?;
    if !read.status.success() {
        let error = String::from_utf8_lossy(&read.stderr);
        return Err(format!("Tesseract could not read it: {}", error.trim()));
    }
    Ok(String::from_utf8_lossy(&read.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG of `width` by `height` pixels, white with a black square.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            if x < width / 2 && y < height / 2 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn decoded(image: &Image) -> DynamicImage {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap();
        image::load_from_memory(&bytes).unwrap()
    }

    #[test]
    fn a_small_screenshot_goes_as_it_is_and_a_large_one_scaled_down() {
        let small = png(800, 600);
        let (image, tokens) = for_model(&small).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .unwrap(),
            small
        );
        assert_eq!(tokens, 800 * 600 / 750);

        let (image, tokens) = for_model(&png(3840, 2160)).unwrap();
        let shown = decoded(&image);
        assert_eq!((shown.width(), shown.height()), (1568, 882));
        assert_eq!(image.media_type, "image/png");
        assert_eq!(tokens, 1568 * 882 / 750);

        assert!(for_model(b"not an image").is_err());
    }

    #[test]
    fn the_text_alone_holds_what_was_read_and_the_text_seen_only_what_is_not_shown() {
        let attached = |name: &str, read: Result<String, String>| Attached {
            name: name.to_owned(),
            path: PathBuf::from(format!("/files/{name}")),
            shown: Err("not prepared".into()),
            read,
        };
        let images = [
            attached("error.png", Ok("Access denied\nuse ```sudo```".into())),
            attached("blank.png", Ok(String::new())),
            attached("photo.jpg", Err("Tesseract is not installed".into())),
        ];
        assert_eq!(
            text_alone("Why?", &images),
            "Why?\n\n[Image: error.png, uploaded to `/files/error.png`]\nText read from it by OCR:\n````text\n\
             Access denied\nuse ```sudo```\n````\n\n[Image: blank.png, uploaded to `/files/blank.png`]\nNo text \
             read from it by OCR.\n\n[Image: photo.jpg, uploaded to `/files/photo.jpg`]\nIts text could not be \
             read: Tesseract is not installed."
        );
        assert_eq!(
            text_seen("Why?", &images),
            text_alone("Why?", &images)
                .replace("`]\n", "`]\nIt cannot be shown to you: not prepared.\n")
        );
        let shown = Attached {
            shown: Ok((
                Arc::new(Image {
                    media_type: "image/png",
                    data: String::new(),
                }),
                85,
            )),
            ..attached("shown.png", Ok("Seen".into()))
        };
        assert_eq!(
            text_seen("", &[shown]),
            "[Image: shown.png, uploaded to `/files/shown.png`]"
        );
        assert_eq!(cut("abcdé", 4), "abcd\n[…]");
        assert_eq!(cut("abcé", 4), "abc\n[…]");
    }

    /// A step of Windows' reading ends at once when the reading is stopped, cancelled, and gives its result when done.
    #[cfg(windows)]
    #[test]
    fn a_step_of_windows_reading_is_cancelled_once_stopped() {
        use windows_future::IAsyncOperation;
        use windows_ocr::{Signal, finish};

        let (signal, signals) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let endless = IAsyncOperation::<i32>::spawn(move || {
            let _ = released.recv_timeout(Duration::from_secs(60));
            Ok(1)
        });
        let stop = windows_ocr::StopOnDrop(signal.clone());
        drop(stop);
        let started = std::time::Instant::now();
        assert_eq!(
            finish(Ok(endless), &signal, &signals),
            Err("the reading was stopped".to_owned())
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(release);

        let (signal, signals) = std::sync::mpsc::channel::<Signal>();
        let done = IAsyncOperation::<i32>::spawn(|| Ok(7));
        assert_eq!(finish(Ok(done), &signal, &signals), Ok(7));
    }

    /// Reads the text of the image `KELPY_OCR_SAMPLE` names, with the system's OCR: run by hand, with
    /// `--ignored --nocapture`, as the OCR depends on the system.
    #[tokio::test]
    #[ignore = "needs an image with text, and the system's OCR"]
    async fn the_text_of_a_screenshot_is_read() {
        let sample = std::env::var("KELPY_OCR_SAMPLE").expect("KELPY_OCR_SAMPLE");
        let text = read_text(Path::new(&sample)).await.unwrap();
        println!("{text}");
        assert!(!text.trim().is_empty());
    }

    #[tokio::test]
    async fn only_uploaded_images_are_attached() {
        let uploads = tempfile::tempdir().unwrap();
        std::fs::write(uploads.path().join("shot.png"), png(40, 30)).unwrap();
        std::fs::write(uploads.path().join("notes.txt"), "text").unwrap();
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|&name| name.to_owned())
                .collect::<Vec<_>>()
        };

        let attached = attach(uploads.path(), &names(&["shot.png"])).await.unwrap();
        assert_eq!(attached[0].name, "shot.png");
        assert!(attached[0].shown.is_ok());
        for refused in [&["notes.txt"][..], &["../shot.png"], &["missing.png"]] {
            assert!(
                attach(uploads.path(), &names(refused)).await.is_err(),
                "{refused:?}"
            );
        }
        let many = vec!["shot.png".to_owned(); MAX_IMAGES + 1];
        assert!(attach(uploads.path(), &many).await.is_err());
    }
}
