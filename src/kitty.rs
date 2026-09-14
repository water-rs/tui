//! Raw Kitty graphics protocol support: the pieces `ratatui-image` does not
//! cover — in-place retransmission (streaming frames onto one image id),
//! z-ordering, explicit deletion, and the unicode-placeholder cell format.
//!
//! Placeholder cells bind an image to the grid: kitty clips them to cell
//! rectangles and scrolls them with the text, which is what lets a video
//! surface live inside a `ScrollView`.
//!
//! Reference: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

use std::cell::{Cell as StdCell, RefCell};
use std::fmt::Write as _;
use std::io::Write as _;
use std::num::NonZeroU16;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

const PLACEHOLDER: char = '\u{10EEEE}';
const UNIT_WIDTH: CellDiffOption = CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap());
const SKIP: CellDiffOption = CellDiffOption::Skip;

/// Base64 chars per APC chunk; kitty wants at most 4096.
const CHUNK_RAW: usize = (4096 / 4) * 3;

/// Pixel encoding used for a [`KittyImage`] transmission.
///
/// `Zlib` sends `o=z,f=32` raw RGBA — the default, ~1ms/frame encode.
/// `Png` sends `f=100` PNG data — ~3–5× smaller on the wire at ~10× the
/// encode cost, worth it when the pty bandwidth is the bottleneck (SSH,
/// high-DPI stills) and frame rate is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransmitFormat {
    /// zlib-compressed RGBA8 (`o=z,f=32`).
    #[default]
    Zlib,
    /// PNG-encoded (`f=100`).
    Png,
}

/// One image slot on the terminal, identified by `id`.
///
/// [`create`](Self::create) transmits pixel data with a virtual placement;
/// [`frame`](Self::frame) retransmits onto the same id. Per the spec, a
/// retransmission deletes the image *and its placements*, so `frame` is also
/// `a=T,U=1` — the virtual placement is recreated atomically in the same
/// command, and placeholder cells bound to the id show the new frame. That is
/// the whole trick behind streaming video.
pub struct KittyImage {
    /// Kitty image id (`i=`).
    pub id: u32,
    /// Pixel width of the transmitted data.
    pub width: u32,
    /// Pixel height of the transmitted data.
    pub height: u32,
    /// Transmission encoding.
    pub format: TransmitFormat,
}

impl KittyImage {
    /// Creates an image slot for `width`×`height` RGBA8 data, zlib-encoded.
    #[must_use]
    pub const fn new(id: u32, width: u32, height: u32) -> Self {
        Self {
            id,
            width,
            height,
            format: TransmitFormat::Zlib,
        }
    }

    /// The same slot transmitting PNG data instead of zlib RGBA.
    #[must_use]
    pub const fn png(mut self) -> Self {
        self.format = TransmitFormat::Png;
        self
    }

    /// Encodes the initial transmission: compressed pixel data plus a virtual
    /// placement (`a=T,U=1`) sized `cols`×`rows` cells — kitty scales the
    /// image into that rectangle wherever placeholder cells for `id` appear.
    /// (z-index does not apply to virtual placements.)
    pub fn create(&self, rgba: &[u8], cols: u16, rows: u16) -> Vec<u8> {
        self.encode(rgba, &format!("a=T,U=1,c={cols},r={rows}"))
    }

    /// Encodes a replacement frame: retransmitting deletes the image and its
    /// virtual placement, so `a=T,U=1` recreates both in one command — the
    /// placeholders bound to `id` then display the new pixels. `cols`/`rows`
    /// must match the placeholder grid currently on screen.
    pub fn frame(&self, rgba: &[u8], cols: u16, rows: u16) -> Vec<u8> {
        self.encode(rgba, &format!("a=T,U=1,c={cols},r={rows}"))
    }

    /// Resizes the virtual placement to `cols`×`rows` cells (`a=p,U=1`)
    /// without retransmitting a single pixel — the terminal rescales the
    /// already-transmitted data. This is what makes window resizes free.
    #[must_use]
    pub fn resize_placement(&self, cols: u16, rows: u16) -> Vec<u8> {
        format!("\x1b_Ga=p,U=1,i={},c={cols},r={rows};\x1b\\", self.id).into_bytes()
    }

    /// Creates a *real* placement (`a=p`) at the current cursor position,
    /// sized `cols`×`rows` cells. `z` orders it: negative values put the
    /// image *below* text so cells with a default background show the image
    /// through — picture-behind-text needs this since z-index does not apply
    /// to virtual placements. Call more than once with different
    /// `placement_id`s to place the same transmitted image in several spots.
    #[must_use]
    pub fn place(&self, placement_id: u32, cols: u16, rows: u16, z: i32) -> Vec<u8> {
        format!(
            "\x1b_Ga=p,i={},p={placement_id},c={cols},r={rows},z={z};\x1b\\",
            self.id
        )
        .into_bytes()
    }

    /// Encodes deletion of the image and all its placements (`a=d`).
    #[must_use]
    pub fn delete(&self) -> Vec<u8> {
        format!("\x1b_Ga=d,d=I,i={};\x1b\\", self.id).into_bytes()
    }

    fn encode(&self, rgba: &[u8], action: &str) -> Vec<u8> {
        let (payload, params) = match self.format {
            TransmitFormat::Zlib => {
                let mut compressed = Vec::with_capacity(rgba.len() / 4);
                ZlibEncoder::new(&mut compressed, Compression::fast())
                    .write_all(rgba)
                    .expect("zlib encoder");
                (compressed, "o=z,f=32")
            }
            TransmitFormat::Png => (png_encode(rgba, self.width, self.height), "f=100"),
        };
        let mut out = String::with_capacity(payload.len() * 4 / 3 + 4096);
        let mut first = true;
        let count = payload.len().div_ceil(CHUNK_RAW).max(1);
        for (i, chunk) in payload.chunks(CHUNK_RAW).enumerate() {
            out.push_str("\x1b_Gq=2,t=d,");
            out.push_str(params);
            out.push(',');
            write!(out, "i={},", self.id).unwrap();
            if first {
                write!(out, "{action},s={},v={},", self.width, self.height).unwrap();
                first = false;
            }
            write!(out, "m={};", u8::from(i + 1 < count)).unwrap();
            base64_simd::STANDARD.encode_append(chunk, &mut out);
            out.push_str("\x1b\\");
        }
        out.into_bytes()
    }
}

fn png_encode(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    image::ImageEncoder::write_image(
        encoder,
        rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .expect("png encoder");
    out
}

/// Draws `cell_width`×`cell_height` placeholder cells for `image` into the
/// buffer at `area`, so kitty renders the image exactly there. The cells are
/// bound to the grid: they scroll and clip like ordinary text.
///
/// `skipped` is the number of grid rows/columns scrolled above/left of the
/// visible area — the placeholder diacritics for those cells are omitted so
/// the image's hidden slice is not painted over the clip.
pub fn draw_placeholders(
    image: &KittyImage,
    cell_width: u16,
    cell_height: u16,
    area: Rect,
    skipped: (u16, u16),
    buf: &mut Buffer,
) {
    let (skipped_cols, skipped_rows) = skipped;
    let max_index = DIACRITICS.len() as u16;
    let full_width = area.width.min(cell_width.saturating_sub(skipped_cols));
    if full_width == 0 {
        return;
    }
    let [id_extra, r, g, b] = image.id.to_be_bytes();
    let id_color = format!("\x1b[38;2;{r};{g};{b}m");
    let id_extra = u16::from(id_extra);

    // Filling a cell with the placeholder char repeated makes kitty render one
    // placeholder per emitted glyph, so a single cell's symbol covers a row.
    let row_tail: String = std::iter::repeat_n(PLACEHOLDER, usize::from(full_width) - 1).collect();
    let restore = format!("\x1b[u\x1b[{}C\x1b[{}B", area.width - 1, area.height - 1);

    let height = area
        .height
        .min(cell_height)
        .min(max_index.saturating_sub(skipped_rows));
    for y in 0..height {
        let mut symbol = String::with_capacity(id_color.len() + full_width as usize * 4 + 40);
        let _ = write!(
            symbol,
            "\x1b[s{id_color}{PLACEHOLDER}{}{}{}",
            diacritic(y + skipped_rows),
            diacritic(skipped_cols),
            diacritic(id_extra),
        );
        symbol.push_str(&row_tail);
        symbol.push_str(&restore);

        for x in 1..full_width {
            if let Some(cell) = buf.cell_mut((area.left() + x, area.top() + y)) {
                cell.set_diff_option(SKIP);
            }
        }
        if let Some(cell) = buf.cell_mut((area.left(), area.top() + y)) {
            cell.set_symbol(&symbol).set_diff_option(UNIT_WIDTH);
        }
    }
}

/// Shared kitty graphics bookkeeping for one app run.
///
/// Nodes ask it for image ids and push raw escape sequences (transmissions,
/// placement updates, deletions) into the outbox; the present step drains the
/// outbox to the terminal writer. This indirection is what lets a node's
/// `Drop` — which cannot reach the terminal itself — still order an image
/// deletion, and what keeps every `i=` unique across the tree.
#[derive(Debug, Default)]
pub struct KittyChannel {
    next_id: StdCell<u32>,
    outbox: RefCell<Vec<Vec<u8>>>,
}

impl KittyChannel {
    /// Allocates a fresh kitty image id (`i=`).
    #[must_use]
    pub fn alloc(&self) -> u32 {
        let id = self.next_id.get().max(1);
        self.next_id.set(id + 1);
        id
    }

    /// Queues raw kitty protocol bytes for emission at the next present.
    pub fn send(&self, bytes: Vec<u8>) {
        self.outbox.borrow_mut().push(bytes);
    }

    /// Drains every queued command. Called once per frame by the app loop.
    #[must_use]
    pub fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbox.borrow_mut())
    }
}

fn diacritic(index: u16) -> char {
    DIACRITICS
        .get(usize::from(index))
        .copied()
        .unwrap_or(DIACRITICS[0])
}

/// Row/column position marks, from kitty's `rowcolumn-diacritics.txt`.
static DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_sends_virtual_placement_with_pixels() {
        let image = KittyImage::new(7, 2, 2);
        let bytes = image.create(&[0u8; 16], 4, 3);
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.starts_with("\u{1b}_Gq=2,t=d,o=z,f=32,i=7,a=T,U=1,c=4,r=3,s=2,v=2,m=0;"));
    }

    #[test]
    fn png_format_sends_f100_without_zlib() {
        let image = KittyImage::new(7, 2, 2).png();
        let bytes = image.create(&[255u8; 16], 4, 3);
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.contains("f=100"));
        assert!(!out.contains("o=z"));
        assert!(out.contains("a=T,U=1,c=4,r=3"));
    }

    #[test]
    fn resize_placement_carries_no_pixels() {
        let image = KittyImage::new(7, 2, 2);
        assert_eq!(
            image.resize_placement(10, 5),
            b"\x1b_Ga=p,U=1,i=7,c=10,r=5;\x1b\\"
        );
    }

    #[test]
    fn place_sets_z_index_and_placement_id() {
        let image = KittyImage::new(7, 2, 2);
        assert_eq!(
            image.place(3, 20, 8, -1),
            b"\x1b_Ga=p,i=7,p=3,c=20,r=8,z=-1;\x1b\\"
        );
    }

    #[test]
    fn delete_targets_the_id() {
        assert_eq!(
            KittyImage::new(42, 1, 1).delete(),
            b"\x1b_Ga=d,d=I,i=42;\x1b\\"
        );
    }

    #[test]
    fn placeholders_offset_by_scrolled_cells() {
        let image = KittyImage::new(7, 4, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 10));
        draw_placeholders(&image, 4, 4, Rect::new(0, 0, 4, 4), (1, 2), &mut buf);
        let symbol = buf[(0, 0)].symbol();
        // Row diacritic = skipped 2, column = skipped 1.
        let expected = format!(
            "{PLACEHOLDER}{}{}{}",
            diacritic(2),
            diacritic(1),
            diacritic(0)
        );
        assert!(symbol.contains(&expected), "{symbol:?}");
    }

    #[test]
    fn channel_allocates_ids_and_drains() {
        let channel = KittyChannel::default();
        assert_eq!(channel.alloc(), 1);
        assert_eq!(channel.alloc(), 2);
        channel.send(vec![1, 2]);
        channel.send(vec![3]);
        assert_eq!(channel.take(), vec![vec![1, 2], vec![3]]);
        assert!(channel.take().is_empty());
    }
}
