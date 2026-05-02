use ampaper::page::PageGeometry;

fn main() {
    let g = PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 100,
        dot_percent: 70,
        width: 5100,
        height: 6600,
        print_border: false,
    };
    println!(
        "dx={} dy={} px={} py={} nx={} ny={} bitmap_w={} bitmap_h={}",
        g.dx(),
        g.dy(),
        g.px(),
        g.py(),
        g.nx(),
        g.ny(),
        g.bitmap_width(),
        g.bitmap_height()
    );
}
