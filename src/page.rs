// Page-level rendering and extraction. Per FORMAT-V1.md §4 and
// `Printer.cpp:501-998` / `Decoder.cpp:259-602`. Converts a list of
// 128-byte blocks into a grayscale pixel bitmap (encoder) and reads
// blocks back from a bitmap by sampling at known cell positions
// (decoder, synthetic-only — full scan-style grid registration with
// histogram peaks lands at M6).
//
// Coordinate convention inside this module is **top-down**: pixel
// (x, y) lives at `bitmap[y * width + x]`. PaperBack 1.10's BMP
// output uses bottom-up rows (BMP convention from Printer.cpp:172);
// that flip is the BMP-file I/O layer's responsibility, not the
// page-layout layer's.
//
// "Black" pixels are 0 by default and "white" are 255, matching the
// 256-grayscale palette PaperBack 1.10 writes for its debug BMPs
// (Printer.cpp:704-708). Encoder callers can pass `black = 64` to
// match Printer.cpp:640's "save to BMP" mode if they want the dark-
// gray-on-white appearance the C source uses for debug bitmaps.

use crate::block::{BLOCK_BYTES, NDOT};
use crate::dot_grid;

/// Cell side length, dots: 32 data dots plus a 3-dot gap (2 leading,
/// 1 trailing) per FORMAT-V1.md §4.2. Maps to `(NDOT+3)` in the C
/// source.
pub const CELL_SIZE_DOTS: usize = NDOT + 3;

/// Decoder threshold for "is this dot black?". Pixels strictly less
/// than this value are treated as black, anything ≥ is white. 128 is
/// the natural midpoint for 8-bit grayscale.
pub const DEFAULT_THRESHOLD: u8 = 128;

/// Default "black" pixel value when rendering to paper. PaperBack
/// 1.10 uses palette index 0 for paper output (`Printer.cpp:615`).
pub const BLACK_PAPER: u8 = 0;

/// "Black" pixel value when rendering to BMP debug output. Dark
/// gray rather than pure black — `Printer.cpp:640` picks 64 so the
/// optical decoder has a high-contrast palette ramp to find peaks
/// reliably on synthetic bitmaps.
pub const BLACK_BMP: u8 = 64;

/// All-white pixel.
pub const WHITE: u8 = 255;

/// Page layout parameters. All values are in printer-resolution
/// pixels except `dpi` and `dot_percent`. Mirrors the relevant
/// fields of PaperBack 1.10's `t_printdata` struct.
#[derive(Clone, Copy, Debug)]
pub struct PageGeometry {
    /// Printer X resolution, dots per inch.
    pub ppix: u32,
    /// Printer Y resolution, dots per inch.
    pub ppiy: u32,
    /// Data dot raster, dots per inch (not pixels per inch). Lower
    /// than `ppix`/`ppiy`; PaperBack 1.10 default is 200, giving
    /// dx = ppix / dpi = 3 at 600-DPI printers.
    pub dpi: u32,
    /// Dot fill, percent of cell (0..=100). Larger = denser ink.
    /// PaperBack 1.10 default is 70.
    pub dot_percent: u8,
    /// Printable area width, printer-pixel units. (Page width minus
    /// left + right margins.)
    pub width: u32,
    /// Printable area height, printer-pixel units.
    pub height: u32,
    /// When true, the rendered bitmap includes the sync-raster
    /// fillblocks around the data grid. See FORMAT-V1.md §4.5 and
    /// `Printer.cpp:858-864`. Required for the M6 scan-decoder to
    /// register the grid; optional for M4 self round-trip.
    pub print_border: bool,
}

impl PageGeometry {
    /// Dot pitch in X (printer pixels per dot), floored at 2 so each
    /// cell is at least 2x2 device pixels per dot.
    #[must_use]
    pub fn dx(&self) -> u32 {
        (self.ppix / self.dpi).max(2)
    }

    /// Dot pitch in Y.
    #[must_use]
    pub fn dy(&self) -> u32 {
        (self.ppiy / self.dpi).max(2)
    }

    /// Dot size in X (printer pixels), at least 1.
    #[must_use]
    pub fn px(&self) -> u32 {
        ((self.dx() * u32::from(self.dot_percent)) / 100).max(1)
    }

    /// Dot size in Y.
    #[must_use]
    pub fn py(&self) -> u32 {
        ((self.dy() * u32::from(self.dot_percent)) / 100).max(1)
    }

    /// Inner padding (pixels) between the bitmap edge and the data
    /// grid. Mirrors `Printer.cpp:670-675`: 16-dot wide when border
    /// is on, 0 otherwise. (PaperBack 1.10 also has a "save to BMP"
    /// 25-pixel default; ampaper unifies on the 16-dot rule for
    /// `print_border == true`.)
    #[must_use]
    pub fn border(&self) -> u32 {
        if self.print_border { self.dx() * 16 } else { 0 }
    }

    /// Number of data cells per row. Mirrors `Printer.cpp:680`.
    #[must_use]
    pub fn nx(&self) -> u32 {
        self.width
            .saturating_sub(self.px())
            .saturating_sub(2 * self.border())
            / (CELL_SIZE_DOTS as u32 * self.dx())
    }

    /// Number of data cell rows. Mirrors `Printer.cpp:681`.
    #[must_use]
    pub fn ny(&self) -> u32 {
        self.height
            .saturating_sub(self.py())
            .saturating_sub(2 * self.border())
            / (CELL_SIZE_DOTS as u32 * self.dy())
    }

    /// Total number of data cells per page.
    #[must_use]
    pub fn cells(&self) -> u32 {
        self.nx() * self.ny()
    }

    /// Final bitmap width in pixels. DWORD-aligned to match BMP
    /// stride conventions (`Printer.cpp:687`).
    #[must_use]
    pub fn bitmap_width(&self) -> u32 {
        let raw = self.nx() * CELL_SIZE_DOTS as u32 * self.dx() + self.px() + 2 * self.border();
        (raw + 3) & !3
    }

    /// Final bitmap height in pixels.
    #[must_use]
    pub fn bitmap_height(&self) -> u32 {
        self.ny() * CELL_SIZE_DOTS as u32 * self.dy() + self.py() + 2 * self.border()
    }

    /// Top-left dot (column 0, row 0) origin in the bitmap, for the
    /// data cell with the given index. Mirrors `Printer.cpp:170-171`
    /// in the top-down coordinate system used in this module.
    #[must_use]
    pub fn block_origin_pixels(&self, cell_index: u32) -> (u32, u32) {
        let col = cell_index % self.nx();
        let row = cell_index / self.nx();
        let x = col * CELL_SIZE_DOTS as u32 * self.dx() + 2 * self.dx() + self.border();
        let y = row * CELL_SIZE_DOTS as u32 * self.dy() + 2 * self.dy() + self.border();
        (x, y)
    }
}

/// One block placed at a specific cell on a page.
#[derive(Clone, Copy, Debug)]
pub struct PlacedBlock {
    pub cell_index: u32,
    pub bytes: [u8; BLOCK_BYTES],
}

/// Render a page: walk each placed block, project its 32x32 dot
/// grid into the bitmap at the cell's origin, draw `px * py` filled
/// dots for every set bit. Cells outside `blocks` are left white;
/// when `geometry.print_border` is true, the surrounding fillblocks
/// (sync raster) are drawn too.
///
/// `black` is the pixel value used for filled dots — pass
/// [`BLACK_PAPER`] (= 0) for paper output or [`BLACK_BMP`] (= 64)
/// to match PaperBack 1.10's BMP-debug palette.
#[must_use]
pub fn render(geometry: &PageGeometry, blocks: &[PlacedBlock], black: u8) -> Vec<u8> {
    let width = geometry.bitmap_width() as usize;
    let height = geometry.bitmap_height() as usize;
    let mut bitmap = vec![WHITE; width * height];

    // Draw cell-boundary grid lines first. Mirrors Printer.cpp:830-855.
    // These are the regular vertical and horizontal black stripes that
    // the scan decoder's peak finder keys on to register the grid;
    // without them an unrotated bitmap looks like sparse scattered
    // dots with no obvious lattice. Drawn whether print_border is on
    // or off — they're part of the wire format either way.
    draw_grid_lines(&mut bitmap, width, height, geometry, black);

    for placed in blocks {
        let grid = dot_grid::block_to_dot_grid(&placed.bytes);
        let (x0, y0) = geometry.block_origin_pixels(placed.cell_index);
        draw_dot_grid(
            &mut bitmap,
            width,
            height,
            x0 as usize,
            y0 as usize,
            &grid,
            geometry.dx() as usize,
            geometry.dy() as usize,
            geometry.px() as usize,
            geometry.py() as usize,
            black,
        );
    }

    if geometry.print_border {
        draw_sync_raster(&mut bitmap, width, height, geometry, black);
    }

    bitmap
}

/// Draw the regular cell-boundary grid lines: nx+1 vertical stripes,
/// ny+1 horizontal stripes, each `px` (vertical) or `py` (horizontal)
/// pixels wide. Mirrors Printer.cpp:830-855.
fn draw_grid_lines(
    bitmap: &mut [u8],
    width: usize,
    height: usize,
    geometry: &PageGeometry,
    black: u8,
) {
    let dx = geometry.dx() as usize;
    let dy = geometry.dy() as usize;
    let px = geometry.px() as usize;
    let py = geometry.py() as usize;
    let nx = geometry.nx() as usize;
    let ny = geometry.ny() as usize;
    let border = geometry.border() as usize;
    let lines_height = if geometry.print_border {
        height
    } else {
        ny * CELL_SIZE_DOTS * dy
    };
    let lines_y_start = if geometry.print_border { 0 } else { border };
    // Vertical lines at columns i*(NDOT+3)*dx + border, i in 0..=nx.
    for i in 0..=nx {
        let x_base = i * CELL_SIZE_DOTS * dx + border;
        for y in lines_y_start..(lines_y_start + lines_height).min(height) {
            for k in 0..px {
                let x = x_base + k;
                if x < width {
                    bitmap[y * width + x] = black;
                }
            }
        }
    }
    let lines_width = if geometry.print_border {
        width
    } else {
        nx * CELL_SIZE_DOTS * dx + px
    };
    let lines_x_start = if geometry.print_border { 0 } else { border };
    // Horizontal lines at rows j*(NDOT+3)*dy + border, j in 0..=ny.
    for j in 0..=ny {
        let y_base = j * CELL_SIZE_DOTS * dy + border;
        for k in 0..py {
            let y = y_base + k;
            if y >= height {
                break;
            }
            for x in lines_x_start..(lines_x_start + lines_width).min(width) {
                bitmap[y * width + x] = black;
            }
        }
    }
}

/// Extract every data cell as raw 128-byte blocks by sampling the
/// center of each dot at known cell positions. Returns a vector
/// indexed by cell — entry `i` corresponds to cell `i`. Cells that
/// were never written render as the inverse-XOR pattern (pure mask
/// bytes); higher layers use CRC + ECC to discriminate real blocks
/// from filler.
///
/// `threshold` selects the black/white cutoff; pass
/// [`DEFAULT_THRESHOLD`] (128) for natural mid-gray.
#[must_use]
pub fn extract(geometry: &PageGeometry, bitmap: &[u8], threshold: u8) -> Vec<[u8; BLOCK_BYTES]> {
    let width = geometry.bitmap_width() as usize;
    let dx = geometry.dx() as usize;
    let dy = geometry.dy() as usize;
    let px = geometry.px() as usize;
    let py = geometry.py() as usize;
    let n = geometry.cells() as usize;

    let mut out = Vec::with_capacity(n);
    for cell_index in 0..n {
        let (x0, y0) = geometry.block_origin_pixels(cell_index as u32);
        let mut grid = [0u32; NDOT];
        for (j, slot) in grid.iter_mut().enumerate() {
            let mut row = 0u32;
            for i in 0..NDOT {
                // Sample the center of the dot region.
                let sx = x0 as usize + i * dx + px / 2;
                let sy = y0 as usize + j * dy + py / 2;
                let pixel = bitmap[sy * width + sx];
                if pixel < threshold {
                    row |= 1u32 << i;
                }
            }
            *slot = row;
        }
        out.push(dot_grid::dot_grid_to_block(&grid));
    }
    out
}

// --- Internals ---------------------------------------------------------------

/// Draw a 32-row dot grid into the bitmap at (x0, y0). Each set bit
/// produces a `px x py` filled rectangle. Pixels outside the bitmap
/// bounds are silently skipped — this lets the sync raster's edge
/// fillblocks "spill" off the bitmap as PaperBack 1.10 does (see
/// `Printer.cpp:227`).
#[allow(clippy::too_many_arguments)]
fn draw_dot_grid(
    bitmap: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    grid: &[u32; NDOT],
    dx: usize,
    dy: usize,
    px: usize,
    py: usize,
    black: u8,
) {
    for (j, &row) in grid.iter().enumerate() {
        for i in 0..NDOT {
            if (row >> i) & 1 == 0 {
                continue;
            }
            let dot_x = x0 + i * dx;
            let dot_y = y0 + j * dy;
            for fy in 0..py {
                let y = dot_y + fy;
                if y >= height {
                    break;
                }
                for fx in 0..px {
                    let x = dot_x + fx;
                    if x >= width {
                        break;
                    }
                    bitmap[y * width + x] = black;
                }
            }
        }
    }
}

/// Sync-raster fillblock pattern for a cell at `(blockx, blocky)`
/// outside the data grid. Mirrors `Printer.cpp:212-220`. Same row
/// bit shape as the dot grid (LSB = column 0), but no XOR scramble
/// is applied because the pattern is the format's regular sync,
/// not encoded data.
fn fillblock_pattern(blockx: i32, blocky: i32, nx: u32, ny: u32) -> [u32; NDOT] {
    let mut grid = [0u32; NDOT];
    let nx_i32 = nx as i32;
    let ny_i32 = ny as i32;
    for (j, slot) in grid.iter_mut().enumerate() {
        *slot = if j & 1 == 0 {
            0x5555_5555
        } else if (blocky < 0 && j <= 24) || (blocky >= ny_i32 && j > 8) {
            // Top/bottom border odd-rows in specific ranges go fully
            // dark — provides directional fiducial for the decoder.
            0
        } else if blockx < 0 {
            0xAA00_0000
        } else if blockx >= nx_i32 {
            0x0000_00AA
        } else {
            0xAAAA_AAAA
        };
    }
    grid
}

/// Draw the one-cell-thick sync-raster border around the data grid.
/// Cells: `(-1, j)` and `(nx, j)` for j in -1..=ny, plus `(i, -1)`
/// and `(i, ny)` for i in 0..nx. Mirrors `Printer.cpp:858-864`.
fn draw_sync_raster(
    bitmap: &mut [u8],
    width: usize,
    height: usize,
    geometry: &PageGeometry,
    black: u8,
) {
    let dx = geometry.dx() as usize;
    let dy = geometry.dy() as usize;
    let px = geometry.px() as usize;
    let py = geometry.py() as usize;
    let nx = geometry.nx();
    let ny = geometry.ny();
    let border = geometry.border() as i32;

    let cell_x = |blockx: i32| -> i32 {
        blockx * CELL_SIZE_DOTS as i32 * dx as i32 + 2 * dx as i32 + border
    };
    let cell_y = |blocky: i32| -> i32 {
        blocky * CELL_SIZE_DOTS as i32 * dy as i32 + 2 * dy as i32 + border
    };

    let mut draw = |bx: i32, by: i32| {
        let grid = fillblock_pattern(bx, by, nx, ny);
        let x0 = cell_x(bx);
        let y0 = cell_y(by);
        // Clip to bitmap bounds: skip individual dots that fall
        // outside, same as PaperBack's Fillblock at Printer.cpp:227.
        for (j, &row) in grid.iter().enumerate() {
            for i in 0..NDOT {
                if (row >> i) & 1 == 0 {
                    continue;
                }
                let base_x = x0 + i as i32 * dx as i32;
                let base_y = y0 + j as i32 * dy as i32;
                for fy in 0..py as i32 {
                    let y = base_y + fy;
                    if y < 0 || y >= height as i32 {
                        continue;
                    }
                    for fx in 0..px as i32 {
                        let x = base_x + fx;
                        if x < 0 || x >= width as i32 {
                            continue;
                        }
                        bitmap[y as usize * width + x as usize] = black;
                    }
                }
            }
        }
    };

    // Left + right border columns (including the four corners via j ∈ -1..=ny).
    for j in -1..=ny as i32 {
        draw(-1, j);
        draw(nx as i32, j);
    }
    // Top + bottom border rows (corners already covered above).
    for i in 0..nx as i32 {
        draw(i, -1);
        draw(i, ny as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, ECC_BYTES, NDATA};

    /// 600-DPI A4-ish geometry. Cell stride is 35 dots × dx=3 = 105
    /// pixels, so an 8.27"×11.69" page (1654×2338 mm at 200 DPI ish)
    /// fits about 14×21 = 294 cells. We pick smaller numbers in
    /// the round-trip tests to keep them fast.
    fn small_geometry(print_border: bool) -> PageGeometry {
        PageGeometry {
            ppix: 600,
            ppiy: 600,
            dpi: 200,
            dot_percent: 70,
            // 12-cell wide × 6-cell tall — 72 cells, plenty for any
            // M4 round-trip without ballooning the bitmap allocation.
            width: 12 * 35 * 3 + 2,
            height: 6 * 35 * 3 + 2,
            print_border,
        }
    }

    #[test]
    fn derived_dimensions_match_format_spec() {
        let g = small_geometry(false);
        assert_eq!(g.dx(), 3);
        assert_eq!(g.dy(), 3);
        assert_eq!(g.px(), 2);
        assert_eq!(g.py(), 2);
        assert_eq!(g.nx(), 12);
        assert_eq!(g.ny(), 6);
        assert_eq!(g.cells(), 72);
    }

    /// A page with no placed blocks still has the cell-boundary grid
    /// lines (added in M6's scan-decoder commit so peak finding has
    /// regular vertical/horizontal stripes to register). Inside any
    /// data cell — i.e. between grid lines, far enough in to skip
    /// the cell's leading-edge dot allocation — the bitmap is white.
    #[test]
    fn empty_page_has_grid_lines_but_white_dot_regions() {
        let g = small_geometry(false);
        let bitmap = render(&g, &[], BLACK_PAPER);
        let width = g.bitmap_width() as usize;
        // Sample the center of dot (15, 15) of cell 0 — well inside
        // the cell, far from any grid line. Must be white since no
        // block was placed.
        let (x0, y0) = g.block_origin_pixels(0);
        let probe_x = x0 as usize + 15 * g.dx() as usize;
        let probe_y = y0 as usize + 15 * g.dy() as usize;
        assert_eq!(bitmap[probe_y * width + probe_x], WHITE);
        // Grid lines themselves are black at known positions.
        // First vertical line is at column `border` for px pixels.
        let line_x = g.border() as usize;
        let probe_y2 = (g.bitmap_height() / 2) as usize;
        assert_eq!(bitmap[probe_y2 * width + line_x], BLACK_PAPER);
    }

    /// A single block placed in cell 0 round-trips through render +
    /// extract byte-for-byte. The simplest proof that the geometry
    /// math is consistent between encoder and decoder.
    #[test]
    fn single_block_round_trips() {
        let g = small_geometry(false);
        let bytes: [u8; BLOCK_BYTES] = core::array::from_fn(|i| (i as u8).wrapping_mul(31));
        let bitmap = render(
            &g,
            &[PlacedBlock {
                cell_index: 0,
                bytes,
            }],
            BLACK_PAPER,
        );
        let extracted = extract(&g, &bitmap, DEFAULT_THRESHOLD);
        assert_eq!(extracted[0], bytes);
    }

    /// Six 90-byte chunks (= 540 bytes capacity, 500 used) round-trip
    /// through real Block construction with proper CRC and ECC. Pins
    /// that the M2 (Block + CRC + ECC) layer composes cleanly with
    /// the M4 page geometry. This is the headline M4 milestone test.
    #[test]
    fn round_trip_500_bytes_at_600_dpi() {
        let g = small_geometry(false);
        let payload: Vec<u8> = (0..500u32).map(|i| i.wrapping_mul(31) as u8).collect();
        let chunks: Vec<&[u8]> = payload.chunks(NDATA).collect();
        assert_eq!(
            chunks.len(),
            6,
            "500 bytes splits into 6 NDATA-sized chunks"
        );

        let mut placed = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            let mut data = [0u8; NDATA];
            data[..chunk.len()].copy_from_slice(chunk);
            let mut block = Block {
                addr: (i * NDATA) as u32,
                data,
                crc: 0,
                ecc: [0; ECC_BYTES],
            };
            block.crc = block.compute_crc();
            block.ecc = block.compute_ecc();
            placed.push(PlacedBlock {
                cell_index: i as u32,
                bytes: block.to_bytes(),
            });
        }

        let bitmap = render(&g, &placed, BLACK_PAPER);
        let extracted = extract(&g, &bitmap, DEFAULT_THRESHOLD);

        // Each placed block must come out byte-exact AND verify both
        // CRC and ECC after extraction.
        for original in &placed {
            let recovered_bytes = &extracted[original.cell_index as usize];
            assert_eq!(
                recovered_bytes, &original.bytes,
                "cell {} bytes diverged",
                original.cell_index
            );
            let block = Block::from_bytes(recovered_bytes);
            assert!(
                block.verify_crc(),
                "cell {} CRC failed",
                original.cell_index
            );
            assert!(
                block.verify_ecc(),
                "cell {} ECC failed",
                original.cell_index
            );
        }

        // Reassemble the 500-byte payload from the six recovered blocks
        // and confirm it matches the input byte-exact.
        let mut reassembled = Vec::with_capacity(500);
        for placed_block in &placed {
            let block = Block::from_bytes(&extracted[placed_block.cell_index as usize]);
            reassembled.extend_from_slice(&block.data);
        }
        reassembled.truncate(500);
        assert_eq!(reassembled, payload);
    }

    /// Sync raster (the alternating 0x55555555 / 0xAAAAAAAA fillblock
    /// pattern) draws cells around the data grid when `print_border`
    /// is on. The data cells inside still round-trip identically — the
    /// raster is purely additional pixels outside their footprint.
    #[test]
    fn sync_raster_does_not_disturb_data_cells() {
        let g_border = small_geometry(true);
        let g_no_border = small_geometry(false);
        let bytes: [u8; BLOCK_BYTES] = core::array::from_fn(|i| (i as u8).wrapping_mul(91));
        let placed = [PlacedBlock {
            cell_index: 5,
            bytes,
        }];

        let bitmap_with = render(&g_border, &placed, BLACK_PAPER);
        let bitmap_without = render(&g_no_border, &placed, BLACK_PAPER);

        let extracted_with = extract(&g_border, &bitmap_with, DEFAULT_THRESHOLD);
        let extracted_without = extract(&g_no_border, &bitmap_without, DEFAULT_THRESHOLD);

        assert_eq!(extracted_with[5], bytes);
        assert_eq!(extracted_without[5], bytes);
    }

    /// Fillblock edge-case patterns from `Printer.cpp:212-220`. Even
    /// rows are always 0x55555555 inside the border. Odd rows pick
    /// from { 0, 0xAA000000, 0x000000AA, 0xAAAAAAAA } based on which
    /// edge the cell sits on.
    #[test]
    fn fillblock_pattern_matches_paperbak_edge_rules() {
        // Above-the-grid cell (blocky < 0): odd rows j ≤ 24 are zero.
        let above = fillblock_pattern(5, -1, 12, 6);
        assert_eq!(above[0], 0x5555_5555, "even row in above-grid cell");
        assert_eq!(above[1], 0, "odd row j=1 in above-grid cell is zero");
        assert_eq!(above[23], 0, "odd row j=23 in above-grid cell is zero");
        assert_eq!(above[25], 0xAAAA_AAAA, "odd row j=25 returns to default");

        // Below-the-grid cell (blocky >= ny): odd rows j > 8 are zero.
        let below = fillblock_pattern(5, 6, 12, 6);
        assert_eq!(
            below[1], 0xAAAA_AAAA,
            "odd row j=1 in below cell stays default"
        );
        assert_eq!(below[7], 0xAAAA_AAAA, "odd row j=7 still default");
        assert_eq!(below[9], 0, "odd row j=9 in below cell is zero");

        // Left-of-grid cell (blockx < 0, in the row range): 0xAA000000.
        let left = fillblock_pattern(-1, 3, 12, 6);
        assert_eq!(left[1], 0xAA00_0000);

        // Right-of-grid cell (blockx >= nx): 0x000000AA.
        let right = fillblock_pattern(12, 3, 12, 6);
        assert_eq!(right[1], 0x0000_00AA);
    }
}
