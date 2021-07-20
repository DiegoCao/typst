use std::cell::RefCell;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use image::{GenericImageView, Rgba};
use tiny_skia as sk;
use ttf_parser::{GlyphId, OutlineBuilder};
use walkdir::WalkDir;

use typst::color::Color;
use typst::diag::{Diag, DiagSet, Level};
use typst::eval::{eval, Scope, Value};
use typst::exec::{exec, State};
use typst::geom::{self, Length, PathElement, Point, Sides, Size};
use typst::image::ImageId;
use typst::layout::{layout, Element, Frame, Geometry, Paint, Text};
use typst::loading::{FileId, FsLoader};
use typst::parse::{parse, LineMap, Scanner};
use typst::syntax::{Location, Pos};
use typst::Context;

const TYP_DIR: &str = "./typ";
const REF_DIR: &str = "./ref";
const PNG_DIR: &str = "./png";
const PDF_DIR: &str = "./pdf";
const FONT_DIR: &str = "../fonts";

fn main() {
    env::set_current_dir(env::current_dir().unwrap().join("tests")).unwrap();

    let args = Args::new(env::args().skip(1));
    let mut filtered = Vec::new();

    for entry in WalkDir::new(".").into_iter() {
        let entry = entry.unwrap();
        if entry.depth() <= 1 {
            continue;
        }

        let src_path = entry.into_path();
        if src_path.extension() != Some(OsStr::new("typ")) {
            continue;
        }

        if args.matches(&src_path.to_string_lossy()) {
            filtered.push(src_path);
        }
    }

    let len = filtered.len();
    if len == 1 {
        println!("Running test ...");
    } else if len > 1 {
        println!("Running {} tests", len);
    }

    // We want to have "unbounded" pages, so we allow them to be infinitely
    // large and fit them to match their content.
    let mut state = State::default();
    state.page.size = Size::new(Length::pt(120.0), Length::inf());
    state.page.margins = Sides::splat(Some(Length::pt(10.0).into()));

    // Create a file system loader.
    let loader = {
        let mut loader = typst::loading::FsLoader::new();
        loader.search_path(FONT_DIR);
        Rc::new(loader)
    };

    let mut ctx = typst::Context::builder().state(state).build(loader.clone());

    let mut ok = true;
    for src_path in filtered {
        let path = src_path.strip_prefix(TYP_DIR).unwrap();
        let png_path = Path::new(PNG_DIR).join(path).with_extension("png");
        let ref_path = Path::new(REF_DIR).join(path).with_extension("png");
        let pdf_path =
            args.pdf.then(|| Path::new(PDF_DIR).join(path).with_extension("pdf"));

        ok &= test(
            loader.as_ref(),
            &mut ctx,
            &src_path,
            &png_path,
            &ref_path,
            pdf_path.as_deref(),
        );
    }

    if !ok {
        std::process::exit(1);
    }
}

struct Args {
    filter: Vec<String>,
    pdf: bool,
    perfect: bool,
}

impl Args {
    fn new(args: impl Iterator<Item = String>) -> Self {
        let mut filter = Vec::new();
        let mut perfect = false;
        let mut pdf = false;

        for arg in args {
            match arg.as_str() {
                "--nocapture" => {}
                "--pdf" => pdf = true,
                "=" => perfect = true,
                _ => filter.push(arg),
            }
        }

        Self { filter, pdf, perfect }
    }

    fn matches(&self, name: &str) -> bool {
        if self.perfect {
            self.filter.iter().any(|p| name == p)
        } else {
            self.filter.is_empty() || self.filter.iter().any(|p| name.contains(p))
        }
    }
}

struct Panic {
    pos: Pos,
    lhs: Option<Value>,
    rhs: Option<Value>,
}

fn test(
    loader: &FsLoader,
    ctx: &mut Context,
    src_path: &Path,
    png_path: &Path,
    ref_path: &Path,
    pdf_path: Option<&Path>,
) -> bool {
    let name = src_path.strip_prefix(TYP_DIR).unwrap_or(src_path);
    println!("Testing {}", name.display());

    let src = fs::read_to_string(src_path).unwrap();
    let src_id = loader.resolve_path(src_path).unwrap();

    let mut ok = true;
    let mut frames = vec![];
    let mut lines = 0;
    let mut compare_ref = true;
    let mut compare_ever = false;

    let parts: Vec<_> = src.split("\n---").collect();
    for (i, part) in parts.iter().enumerate() {
        let is_header = i == 0
            && parts.len() > 1
            && part
                .lines()
                .all(|s| s.starts_with("//") || s.chars().all(|c| c.is_whitespace()));

        if is_header {
            for line in part.lines() {
                if line.starts_with("// Ref: false") {
                    compare_ref = false;
                }
            }
        } else {
            let (part_ok, compare_here, part_frames) =
                test_part(ctx, src_id, part, i, compare_ref, lines);
            ok &= part_ok;
            compare_ever |= compare_here;
            frames.extend(part_frames);
        }

        lines += part.lines().count() as u32 + 1;
    }

    if compare_ever {
        if let Some(pdf_path) = pdf_path {
            let pdf_data = typst::export::pdf(ctx, &frames);
            fs::create_dir_all(&pdf_path.parent().unwrap()).unwrap();
            fs::write(pdf_path, pdf_data).unwrap();
        }

        let canvas = draw(ctx, &frames, 2.0);
        fs::create_dir_all(&png_path.parent().unwrap()).unwrap();
        canvas.save_png(png_path).unwrap();

        if let Ok(ref_pixmap) = sk::Pixmap::load_png(ref_path) {
            if canvas != ref_pixmap {
                println!("  Does not match reference image. ❌");
                ok = false;
            }
        } else {
            println!("  Failed to open reference image. ❌");
            ok = false;
        }
    }

    if ok {
        println!("\x1b[1ATesting {} ✔", name.display());
    }

    ok
}

fn test_part(
    ctx: &mut Context,
    src_id: FileId,
    src: &str,
    i: usize,
    compare_ref: bool,
    lines: u32,
) -> (bool, bool, Vec<Rc<Frame>>) {
    let map = LineMap::new(src);
    let (local_compare_ref, ref_diags) = parse_metadata(src, &map);
    let compare_ref = local_compare_ref.unwrap_or(compare_ref);

    // We hook up some extra test helpers into the global scope.
    let mut scope = typst::library::new();
    let panics = Rc::new(RefCell::new(vec![]));
    register_helpers(&mut scope, Rc::clone(&panics));

    let parsed = parse(src);
    let evaluated = eval(ctx, src_id, Rc::new(parsed.output));
    let executed = exec(ctx, &evaluated.output.template);
    let mut layouted = layout(ctx, &executed.output);

    let mut diags = parsed.diags;
    diags.extend(evaluated.diags);
    diags.extend(executed.diags);

    let mut ok = true;

    for panic in &*panics.borrow() {
        let line = map.location(panic.pos).unwrap().line;
        println!("  Assertion failed in line {} ❌", lines + line);
        if let (Some(lhs), Some(rhs)) = (&panic.lhs, &panic.rhs) {
            println!("    Left:  {:?}", lhs);
            println!("    Right: {:?}", rhs);
        } else {
            println!("    Missing argument.");
        }
        ok = false;
    }

    if diags != ref_diags {
        println!("  Subtest {} does not match expected diagnostics. ❌", i);
        ok = false;

        for diag in &diags {
            if !ref_diags.contains(diag) {
                print!("    Not annotated | ");
                print_diag(diag, &map, lines);
            }
        }

        for diag in &ref_diags {
            if !diags.contains(diag) {
                print!("    Not emitted   | ");
                print_diag(diag, &map, lines);
            }
        }
    }

    #[cfg(feature = "layout-cache")]
    {
        let reference = ctx.layouts.clone();
        for level in 0 .. reference.levels() {
            ctx.layouts = reference.clone();
            ctx.layouts.retain(|x| x == level);
            if ctx.layouts.frames.is_empty() {
                continue;
            }

            ctx.layouts.turnaround();

            let cached_result = layout(ctx, &executed.output);

            let misses = ctx
                .layouts
                .frames
                .iter()
                .flat_map(|(_, e)| e)
                .filter(|e| e.level == level && !e.hit() && e.age() == 2)
                .count();

            if misses > 0 {
                ok = false;
                println!(
                    "    Recompilation had {} cache misses on level {} (Subtest {}) ❌",
                    misses, level, i
                );
            }

            if cached_result != layouted {
                ok = false;
                println!(
                    "    Recompilation of subtest {} differs from clean pass ❌",
                    i
                );
            }
        }

        ctx.layouts = reference;
        ctx.layouts.turnaround();
    }

    if !compare_ref {
        layouted.clear();
    }

    (ok, compare_ref, layouted)
}

fn parse_metadata(src: &str, map: &LineMap) -> (Option<bool>, DiagSet) {
    let mut diags = DiagSet::new();
    let mut compare_ref = None;

    let lines: Vec<_> = src.lines().map(str::trim).collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("// Ref: false") {
            compare_ref = Some(false);
        }

        if line.starts_with("// Ref: true") {
            compare_ref = Some(true);
        }

        let (level, rest) = if let Some(rest) = line.strip_prefix("// Warning: ") {
            (Level::Warning, rest)
        } else if let Some(rest) = line.strip_prefix("// Error: ") {
            (Level::Error, rest)
        } else {
            continue;
        };

        fn num(s: &mut Scanner) -> u32 {
            s.eat_while(|c| c.is_numeric()).parse().unwrap()
        }

        let comments =
            lines[i ..].iter().take_while(|line| line.starts_with("//")).count();

        let pos = |s: &mut Scanner| -> Pos {
            let first = num(s);
            let (delta, column) =
                if s.eat_if(':') { (first, num(s)) } else { (1, first) };
            let line = (i + comments) as u32 + delta;
            map.pos(Location::new(line, column)).unwrap()
        };

        let mut s = Scanner::new(rest);
        let start = pos(&mut s);
        let end = if s.eat_if('-') { pos(&mut s) } else { start };

        diags.insert(Diag::new(start .. end, level, s.rest().trim()));
    }

    (compare_ref, diags)
}

fn register_helpers(scope: &mut Scope, panics: Rc<RefCell<Vec<Panic>>>) {
    scope.def_const("error", Value::Error);

    scope.def_func("args", |_, args| {
        let repr = typst::pretty::pretty(args);
        args.items.clear();
        Value::template(move |ctx| {
            ctx.set_monospace();
            ctx.push_text(&repr);
        })
    });

    scope.def_func("test", move |ctx, args| {
        let lhs = args.expect::<Value>(ctx, "left-hand side");
        let rhs = args.expect::<Value>(ctx, "right-hand side");
        if lhs != rhs {
            panics.borrow_mut().push(Panic { pos: args.span.start, lhs, rhs });
        }
        Value::None
    });
}

fn print_diag(diag: &Diag, map: &LineMap, lines: u32) {
    let mut start = map.location(diag.span.start).unwrap();
    let mut end = map.location(diag.span.end).unwrap();
    start.line += lines;
    end.line += lines;
    println!("{}: {}-{}: {}", diag.level, start, end, diag.message);
}

fn draw(ctx: &Context, frames: &[Rc<Frame>], dpi: f32) -> sk::Pixmap {
    let pad = Length::pt(5.0);

    let height = pad + frames.iter().map(|l| l.size.height + pad).sum::<Length>();
    let width = 2.0 * pad + frames.iter().map(|l| l.size.width).max().unwrap_or_default();

    let pixel_width = (dpi * width.to_pt() as f32) as u32;
    let pixel_height = (dpi * height.to_pt() as f32) as u32;
    if pixel_width > 4000 || pixel_height > 4000 {
        panic!(
            "overlarge image: {} by {} ({} x {})",
            pixel_width, pixel_height, width, height,
        );
    }

    let mut canvas = sk::Pixmap::new(pixel_width, pixel_height).unwrap();
    let ts = sk::Transform::from_scale(dpi, dpi);
    canvas.fill(sk::Color::BLACK);

    let mut origin = Point::splat(pad);
    for frame in frames {
        let mut paint = sk::Paint::default();
        paint.set_color(sk::Color::WHITE);
        canvas.fill_rect(
            sk::Rect::from_xywh(
                origin.x.to_pt() as f32,
                origin.y.to_pt() as f32,
                frame.size.width.to_pt() as f32,
                frame.size.height.to_pt() as f32,
            )
            .unwrap(),
            &paint,
            ts,
            None,
        );

        for (pos, element) in frame.elements() {
            let global = origin + pos;
            let x = global.x.to_pt() as f32;
            let y = global.y.to_pt() as f32;
            let ts = ts.pre_translate(x, y);
            match *element {
                Element::Text(ref text) => {
                    draw_text(&mut canvas, ts, ctx, text);
                }
                Element::Geometry(ref geometry, paint) => {
                    draw_geometry(&mut canvas, ts, geometry, paint);
                }
                Element::Image(id, size) => {
                    draw_image(&mut canvas, ts, ctx, id, size);
                }
            }
        }

        origin.y += frame.size.height + pad;
    }

    canvas
}

fn draw_text(canvas: &mut sk::Pixmap, ts: sk::Transform, ctx: &Context, text: &Text) {
    let ttf = ctx.fonts.get(text.face_id).ttf();
    let mut x = 0.0;

    for glyph in &text.glyphs {
        let units_per_em = ttf.units_per_em();
        let s = text.size.to_pt() as f32 / units_per_em as f32;
        let dx = glyph.x_offset.to_pt() as f32;
        let ts = ts.pre_translate(x + dx, 0.0);

        // Try drawing SVG if present.
        if let Some(tree) = ttf
            .glyph_svg_image(GlyphId(glyph.id))
            .and_then(|data| std::str::from_utf8(data).ok())
            .map(|svg| {
                let viewbox = format!("viewBox=\"0 0 {0} {0}\" xmlns", units_per_em);
                svg.replace("xmlns", &viewbox)
            })
            .and_then(|s| usvg::Tree::from_str(&s, &usvg::Options::default()).ok())
        {
            for child in tree.root().children() {
                if let usvg::NodeKind::Path(node) = &*child.borrow() {
                    let path = convert_usvg_path(&node.data);
                    let ts = convert_usvg_transform(node.transform)
                        .post_scale(s, s)
                        .post_concat(ts);
                    if let Some(fill) = &node.fill {
                        let (paint, fill_rule) = convert_usvg_fill(fill);
                        canvas.fill_path(&path, &paint, fill_rule, ts, None);
                    }
                }
            }
        } else {
            // Otherwise, draw normal outline.
            let mut builder = WrappedPathBuilder(sk::PathBuilder::new());
            if ttf.outline_glyph(GlyphId(glyph.id), &mut builder).is_some() {
                let path = builder.0.finish().unwrap();
                let ts = ts.pre_scale(s, -s);
                let mut paint = convert_typst_paint(text.fill);
                paint.anti_alias = true;
                canvas.fill_path(&path, &paint, sk::FillRule::default(), ts, None);
            }
        }

        x += glyph.x_advance.to_pt() as f32;
    }
}

fn draw_geometry(
    canvas: &mut sk::Pixmap,
    ts: sk::Transform,
    geometry: &Geometry,
    paint: Paint,
) {
    let paint = convert_typst_paint(paint);
    let rule = sk::FillRule::default();

    match *geometry {
        Geometry::Rect(Size { width, height }) => {
            let w = width.to_pt() as f32;
            let h = height.to_pt() as f32;
            let rect = sk::Rect::from_xywh(0.0, 0.0, w, h).unwrap();
            canvas.fill_rect(rect, &paint, ts, None);
        }
        Geometry::Ellipse(size) => {
            let path = convert_typst_path(&geom::Path::ellipse(size));
            canvas.fill_path(&path, &paint, rule, ts, None);
        }
        Geometry::Line(target, thickness) => {
            let path = {
                let mut builder = sk::PathBuilder::new();
                builder.line_to(target.x.to_pt() as f32, target.y.to_pt() as f32);
                builder.finish().unwrap()
            };

            let mut stroke = sk::Stroke::default();
            stroke.width = thickness.to_pt() as f32;
            canvas.stroke_path(&path, &paint, &stroke, ts, None);
        }
        Geometry::Path(ref path) => {
            let path = convert_typst_path(path);
            canvas.fill_path(&path, &paint, rule, ts, None);
        }
    };
}

fn draw_image(
    canvas: &mut sk::Pixmap,
    ts: sk::Transform,
    ctx: &Context,
    id: ImageId,
    size: Size,
) {
    let img = ctx.images.get(id);

    let mut pixmap = sk::Pixmap::new(img.buf.width(), img.buf.height()).unwrap();
    for ((_, _, src), dest) in img.buf.pixels().zip(pixmap.pixels_mut()) {
        let Rgba([r, g, b, a]) = src;
        *dest = sk::ColorU8::from_rgba(r, g, b, a).premultiply();
    }

    let view_width = size.width.to_pt() as f32;
    let view_height = size.height.to_pt() as f32;
    let scale_x = view_width as f32 / pixmap.width() as f32;
    let scale_y = view_height as f32 / pixmap.height() as f32;

    let mut paint = sk::Paint::default();
    paint.shader = sk::Pattern::new(
        pixmap.as_ref(),
        sk::SpreadMode::Pad,
        sk::FilterQuality::Bilinear,
        1.0,
        sk::Transform::from_row(scale_x, 0.0, 0.0, scale_y, 0.0, 0.0),
    );

    let rect = sk::Rect::from_xywh(0.0, 0.0, view_width, view_height).unwrap();
    canvas.fill_rect(rect, &paint, ts, None);
}

fn convert_typst_paint(paint: Paint) -> sk::Paint<'static> {
    let Paint::Color(Color::Rgba(c)) = paint;
    let mut paint = sk::Paint::default();
    paint.set_color_rgba8(c.r, c.g, c.b, c.a);
    paint
}

fn convert_typst_path(path: &geom::Path) -> sk::Path {
    let mut builder = sk::PathBuilder::new();
    let f = |v: Length| v.to_pt() as f32;
    for elem in &path.0 {
        match elem {
            PathElement::MoveTo(p) => {
                builder.move_to(f(p.x), f(p.y));
            }
            PathElement::LineTo(p) => {
                builder.line_to(f(p.x), f(p.y));
            }
            PathElement::CubicTo(p1, p2, p3) => {
                builder.cubic_to(f(p1.x), f(p1.y), f(p2.x), f(p2.y), f(p3.x), f(p3.y));
            }
            PathElement::ClosePath => {
                builder.close();
            }
        };
    }
    builder.finish().unwrap()
}

fn convert_usvg_transform(transform: usvg::Transform) -> sk::Transform {
    let g = |v: f64| v as f32;
    let usvg::Transform { a, b, c, d, e, f } = transform;
    sk::Transform::from_row(g(a), g(b), g(c), g(d), g(e), g(f))
}

fn convert_usvg_fill(fill: &usvg::Fill) -> (sk::Paint<'static>, sk::FillRule) {
    let mut paint = sk::Paint::default();
    paint.anti_alias = true;

    match fill.paint {
        usvg::Paint::Color(usvg::Color { red, green, blue }) => {
            paint.set_color_rgba8(red, green, blue, fill.opacity.to_u8())
        }
        usvg::Paint::Link(_) => {}
    }

    let rule = match fill.rule {
        usvg::FillRule::NonZero => sk::FillRule::Winding,
        usvg::FillRule::EvenOdd => sk::FillRule::EvenOdd,
    };

    (paint, rule)
}

fn convert_usvg_path(path: &usvg::PathData) -> sk::Path {
    let mut builder = sk::PathBuilder::new();
    let f = |v: f64| v as f32;
    for seg in path.iter() {
        match *seg {
            usvg::PathSegment::MoveTo { x, y } => {
                builder.move_to(f(x), f(y));
            }
            usvg::PathSegment::LineTo { x, y } => {
                builder.line_to(f(x), f(y));
            }
            usvg::PathSegment::CurveTo { x1, y1, x2, y2, x, y } => {
                builder.cubic_to(f(x1), f(y1), f(x2), f(y2), f(x), f(y));
            }
            usvg::PathSegment::ClosePath => {
                builder.close();
            }
        }
    }
    builder.finish().unwrap()
}

struct WrappedPathBuilder(sk::PathBuilder);

impl OutlineBuilder for WrappedPathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.0.close();
    }
}
