//! 着色器族效果:参考 Picasa 3 的 `Shader` 类效果共 10 项,本轮落地算法口径明确的 6 项。
//!
//! 六项共用三条约定(与参考件一致):
//!
//! 1. **逐像素重映射**:目标像素取自「进入本效果时的同一张图」,所以先克隆再写回,
//!    不会出现边写边读的连锁位移。
//! 2. **采样 = 钳位 + 取整**:坐标先钳进 `[0, 宽-1]` / `[0, 高-1]`,再四舍五入取整,
//!    全程不做插值 —— 结果是「原图色彩的搬运」,不产生新色值(各效果的测试都锁了这条)。
//! 3. **参数越界的两种处理**:有的效果直接整幅恒等返回(格长、角度出界),
//!    有的钳到边界值再用(波纹块宽、HSL 三根轴)。逐条注明,不做「悄悄放宽」。
//!
//! 其余 4 项(自然饱和度 / 涂抹 / 催眠 / 可调阈值)算法仍有含糊处,暂不落地,
//! 见 [`crate::effects::SHADER_PENDING`]。

use image::{Rgba, RgbaImage};

/// √3,六边形格的行距系数。
const SQRT3: f32 = 1.7320508;
/// √3/2,第二套六边形格的错位系数。
const HALF_SQRT3: f32 = 0.8660254;

/// 参考件的钳位口径:`v` 落在 `[0, max]` 内原样,越上界取 `max`,越下界取 `0`。
///
/// 不是随手写的 —— 非数(`NaN`)会落到「下界」一侧,与参考件把非法坐标读成左上角一致。
fn clamp_lo_hi(v: f32, max: f32) -> f32 {
    if v >= 0.0 {
        if v <= max {
            v
        } else {
            max
        }
    } else {
        0.0
    }
}

/// 对称轴上的钳位:`v` 越界时取 `±bound`(非数取下界)。
fn clamp_sym(v: f32, bound: f32) -> f32 {
    if v >= -bound {
        if v <= bound {
            v
        } else {
            bound
        }
    } else {
        -bound
    }
}

/// 波纹块宽钳进 `1..100`(非数落到下界 1)。
fn clamp_grid(v: f32) -> f32 {
    if v >= 1.0 {
        if v > 100.0 {
            100.0
        } else {
            v
        }
    } else {
        1.0
    }
}

/// 钳位 + 取整后的采样点。
fn sampled(src: &RgbaImage, x: f32, y: f32) -> Rgba<u8> {
    let (w, h) = src.dimensions();
    let mx = w.saturating_sub(1) as f32;
    let my = h.saturating_sub(1) as f32;
    let ix = clamp_lo_hi(x, mx).round() as u32;
    let iy = clamp_lo_hi(y, my).round() as u32;
    *src.get_pixel(ix.min(mx as u32), iy.min(my as u32))
}

/// 六边形单元化(HexCells)。
///
/// `cell` 是格长(像素)。`cell < 1` 时整幅恒等,与参考件一致。
///
/// 两套错位格:格 A 的格心在 `(W/2, H/2)` 的整数格点上,行距 `cell·√3`;
/// 格 B 在 A 的基础上右移 `cell/2`、下移 `cell·√3/2`。每个像素取两格里
/// **离自己更近的格心**那个像素(平局取格 B),块内不取均值。
pub fn hexcells(img: &mut RgbaImage, cell: f32) {
    if cell < 1.0 {
        return;
    }
    let (w, h) = img.dimensions();
    let src = img.clone();
    let cx = (w / 2) as f32;
    let cy = (h / 2) as f32;
    let row = cell * SQRT3;
    // 格 B 的基点:相对格 A 错位半个格长、半个行距(√3/2·cell)。
    let bx = cx + cell * 0.5;
    let by = cy + cell * HALF_SQRT3;
    for y in 0..h {
        let fy = y as f32;
        for x in 0..w {
            let fx = x as f32;
            let ax = cx + cell * ((fx - cx) / cell + 0.5).floor() - fx;
            let ay = cy + row * ((fy - cy) / row + 0.5).floor() - fy;
            let c_x = bx + cell * ((fx - bx) / cell + 0.5).floor() - fx;
            let c_y = by + row * ((fy - by) / row + 0.5).floor() - fy;
            let (dx, dy) = if c_x * c_x + c_y * c_y <= ax * ax + ay * ay {
                (c_x, c_y)
            } else {
                (ax, ay)
            };
            img.put_pixel(x, y, sampled(&src, fx + dx, fy + dy));
        }
    }
}

/// 块状波纹(RippleBlocks)。
///
/// `ax`/`ay` 是横/纵振幅(像素),`bx`/`by` 是横/纵块宽(钳进 `1..100`),
/// `ph_x`/`ph_y` 是相位(弧度)。位移公式:
/// `sx = x + ax·sin(x/bx + ph_x)`、`sy = y + ay·cos(y/by + ph_y)`。
///
/// 名字里的「块」在参考 Picasa 3只是频率的分母,并没有真的分块。
pub fn ripple_blocks(img: &mut RgbaImage, ax: f32, ay: f32, bx: f32, by: f32, ph_x: f32, ph_y: f32) {
    let (w, h) = img.dimensions();
    let src = img.clone();
    let bx = clamp_grid(bx);
    let by = clamp_grid(by);
    for y in 0..h {
        let fy = y as f32;
        let sy = fy + ay * (fy / by + ph_y).cos();
        for x in 0..w {
            let fx = x as f32;
            let sx = fx + ax * (fx / bx + ph_x).sin();
            img.put_pixel(x, y, sampled(&src, sx, sy));
        }
    }
}

/// 圆周边缘外翻(CircleSplash)。
///
/// 圆心取 `(W·fx, H·fy)`,`radius` 是圆周半径(像素)。
/// 圆**外**的像素全部改成「同角度、圆周上」那个像素,圆**内**(含圆周本身)保持原样 ——
/// 于是圆周边像水花一样向外甩成一圈放射条纹。
///
/// `radius` 出界(负数或大于 300)、圆心落在图外时整幅恒等。
pub fn circle_splash(img: &mut RgbaImage, fx: f32, fy: f32, radius: f32) {
    let (w, h) = img.dimensions();
    if !(0.0..=300.0).contains(&radius) {
        return;
    }
    let (fw, fh) = (w as f32, h as f32);
    let cx = fw * fx;
    let cy = fh * fy;
    if !(0.0..fw).contains(&cx) || !(0.0..fh).contains(&cy) {
        return;
    }
    let src = img.clone();
    for y in 0..h {
        let dy = y as f32 - cy;
        for x in 0..w {
            let dx = x as f32 - cx;
            let r = (dx * dx + dy * dy).sqrt();
            if r < radius {
                continue;
            }
            let k = radius / r;
            img.put_pixel(x, y, sampled(&src, cx + dx * k, cy + dy * k));
        }
    }
}

/// 晶格化(Crystallize)。
///
/// `cell` 是格长(像素),出界(`> 300` 或 `< 1`)时整幅恒等。
///
/// 三套格网各给一个候选格心,取离当前像素最近的:**①** 转 18°(轴 0.951/0.309)、
/// 基点 (2400, −100);**②** 转 30°(轴 0.866/0.5)、基点 (−100, 2400);
/// **③** 不旋转、格长乘 0.95。比较顺序是 ②/① 先淘汰一个,再与 ③ 比 ——
/// 平局保留先选的那个。三套格网叠在一起,块边界才不像规整方格。
pub fn crystallize(img: &mut RgbaImage, cell: f32) {
    let (w, h) = img.dimensions();
    if !(1.0..=300.0).contains(&cell) {
        return;
    }
    let src = img.clone();
    let c3 = cell * 0.95;
    for y in 0..h {
        let fy = y as f32;
        for x in 0..w {
            let fx = x as f32;
            // ① 18° 格网
            let (px, py) = (fx - 2400.0, fy + 100.0);
            let i = ((py * -0.309 + px * 0.951) / cell).floor() + 0.5;
            let j = ((px * 0.309 + py * 0.951) / cell).floor() + 0.5;
            let one = (i * cell * 0.951 + j * cell * 0.309 + 2400.0, i * cell * -0.309 + j * cell * 0.951 - 100.0);
            // ② 30° 格网
            let (px, py) = (fx + 100.0, fy - 2400.0);
            let i = ((py * -0.5 + px * 0.866) / cell).floor() + 0.5;
            let j = ((px * 0.5 + py * 0.866) / cell).floor() + 0.5;
            let two = (i * cell * 0.866 + j * cell * 0.5 - 100.0, i * cell * -0.5 + j * cell * 0.866 + 2400.0);
            // ②/① 取近(平局取 ①)
            let d1 = (one.0 - fx) * (one.0 - fx) + (one.1 - fy) * (one.1 - fy);
            let d2 = (two.0 - fx) * (two.0 - fx) + (two.1 - fy) * (two.1 - fy);
            let (mut bx, mut by) = if d1 <= d2 { one } else { two };
            // ③ 无旋转格网
            let three = (
                c3 * ((fx / c3).floor() + 0.5),
                c3 * ((fy / c3).floor() + 0.5),
            );
            let db = (bx - fx) * (bx - fx) + (by - fy) * (by - fy);
            let d3 = (three.0 - fx) * (three.0 - fx) + (three.1 - fy) * (three.1 - fy);
            if d3 < db {
                bx = three.0;
                by = three.1;
            }
            img.put_pixel(x, y, sampled(&src, bx, by));
        }
    }
}

/// 瀑布折叠(Waterfall)。
///
/// 折叠线取 `(W·fx, H·fy)`,`angle` 决定把**哪一侧**的内容塌到线上(度):
/// `0..90` 上线上方的行、`90..180` 下线右侧的列、`180..270` 下线下方的行、
/// `270` 下线左侧的列。另一侧保持原样,线本身不变 —— **没有镜像**,
/// 所以看到的是「一边被拽到线上」而不是对折。
///
/// `angle` 出界(负数或大于 270)、折叠线落在图外时整幅恒等。
pub fn waterfall(img: &mut RgbaImage, fx: f32, fy: f32, angle: f32) {
    let (w, h) = img.dimensions();
    if !(0.0..=270.0).contains(&angle) {
        return;
    }
    let (fw, fh) = (w as f32, h as f32);
    let lx = fw * fx;
    let ly = fh * fy;
    if !(0.0..fw).contains(&lx) || !(0.0..fh).contains(&ly) {
        return;
    }
    let src = img.clone();
    for y in 0..h {
        let fyv = y as f32;
        for x in 0..w {
            let fxv = x as f32;
            let (sx, sy) = if angle < 90.0 {
                if fyv < ly {
                    (fxv, ly)
                } else {
                    (fxv, fyv)
                }
            } else if angle < 180.0 {
                if fxv >= lx {
                    (lx, fyv)
                } else {
                    (fxv, fyv)
                }
            } else if angle < 270.0 {
                if fyv >= ly {
                    (fxv, ly)
                } else {
                    (fxv, fyv)
                }
            } else if fxv < lx {
                (lx, fyv)
            } else {
                (fxv, fyv)
            };
            img.put_pixel(x, y, sampled(&src, sx, sy));
        }
    }
}

/// HSL 三通道滤镜(HslFilter)。
///
/// `hue` 是色相偏移(度,钳进 ±180,注意参考件是**减去**这个值),
/// `sat` 是饱和度百分比偏移(钳进 ±100,乘性:`S' = S·(1 + sat/100)`),
/// `lum` 是亮度百分比偏移(钳进 ±100,加性:`L' = L + lum/100`)。
///
/// 换算走参考件那套标准 60° 扇区的 HSL,不引入我方其它效果的色彩模型;
/// 透明度原样透传,四个通道各自钳回 `[0,1]` 后取整回 8 位。
pub fn hsl_filter(img: &mut RgbaImage, hue: f32, sat: f32, lum: f32) {
    let hue = clamp_sym(hue, 180.0);
    let sat = clamp_sym(sat, 100.0);
    let lum = clamp_sym(lum, 100.0);
    for p in img.pixels_mut() {
        let r = p[0] as f32 / 255.0;
        let g = p[1] as f32 / 255.0;
        let b = p[2] as f32 / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let light = (max + min) * 0.5;
        // 色相:参考件只在「红为最大」那一支把角绕回 [0,360),另两支不绕 ——
        // 另外两支的取值本来就落在 [60,180] / [180,300],绕不绕都一样。
        let mut h = if max == min {
            0.0
        } else if r == max {
            let mut v = (g - b) / delta * 60.0;
            while v >= 360.0 {
                v -= 360.0;
            }
            while v < 0.0 {
                v += 360.0;
            }
            v
        } else if g == max {
            (b - r) / delta * 60.0 + 120.0
        } else {
            (r - g) / delta * 60.0 + 240.0
        };
        let s = if max == min {
            0.0
        } else if light > 0.5 {
            delta / (2.0 - (max + min))
        } else {
            delta / (max + min)
        };
        h -= hue;
        let s = s * (sat / 100.0 + 1.0);
        let l = light + lum / 100.0;
        let q = if l >= 0.5 {
            (s + l) - s * l
        } else {
            (s + 1.0) * l
        };
        let pp = l * 2.0 - q;
        let base = h / 360.0;
        let mut rgb = [
            hue_channel(wrap(base + 1.0 / 3.0), q, pp),
            hue_channel(wrap(base), q, pp),
            hue_channel(wrap(base - 1.0 / 3.0), q, pp),
        ];
        for v in rgb.iter_mut() {
            *v = clamp_lo_hi(*v, 1.0);
        }
        p[0] = (rgb[0] * 255.0).round() as u8;
        p[1] = (rgb[1] * 255.0).round() as u8;
        p[2] = (rgb[2] * 255.0).round() as u8;
    }
}

/// 把色相分量绕回一圈(参考件只补/减一次,不是取模)。
fn wrap(t: f32) -> f32 {
    if t >= 0.0 {
        if t > 1.0 {
            t - 1.0
        } else {
            t
        }
    } else {
        t + 1.0
    }
}

/// 单个通道的扇区混合。
fn hue_channel(t: f32, q: f32, p: f32) -> f32 {
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * 6.0 * (2.0 / 3.0 - t)
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一张每个像素色值都不重复的渐变图,用来分辨「搬运」还是「混色」。
    fn gradient(w: u32, h: u32) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(
                    x,
                    y,
                    Rgba([
                        (x * 7 + y * 3) as u8 % 251,
                        (x * 13 + 5) as u8 % 253,
                        (y * 17 + 9) as u8 % 249,
                        255,
                    ]),
                );
            }
        }
        img
    }

    /// 采样类效果的共同底线:结果里不许出现原图没有的色值。
    fn assert_only_existing_colors(out: &RgbaImage, src: &RgbaImage) {
        let mut palette: std::collections::HashSet<[u8; 4]> = std::collections::HashSet::new();
        for p in src.pixels() {
            palette.insert(p.0);
        }
        for (i, p) in out.pixels().enumerate() {
            assert!(
                palette.contains(&p.0),
                "第 {i} 个像素 {:?} 不是原图里的色值",
                p.0
            );
        }
    }

    #[test]
    fn hexcells_keeps_grid_centre_and_replaces_its_neighbour() {
        let src = gradient(16, 16);
        let mut img = src.clone();
        hexcells(&mut img, 4.0);
        // 格心 (W/2, H/2) 采样到自己 → 颜色不变
        assert_eq!(
            img.get_pixel(8, 8).0,
            src.get_pixel(8, 8).0,
            "格心应保持自身颜色"
        );
        // 右邻一格:离格 A 中心更近,应被换成格心颜色
        assert_eq!(
            img.get_pixel(9, 8).0,
            src.get_pixel(8, 8).0,
            "紧邻格心的像素应被换成格心颜色"
        );
        assert_ne!(img.as_raw(), src.as_raw(), "整体应被单元化");
        assert_only_existing_colors(&img, &src);
    }

    #[test]
    fn hexcells_below_unit_grid_is_identity() {
        let src = gradient(16, 16);
        let mut img = src.clone();
        hexcells(&mut img, 0.5);
        assert_eq!(img.as_raw(), src.as_raw(), "格长小于 1 时应整幅恒等");
    }

    #[test]
    fn ripple_blocks_shifts_only_the_amplified_axis() {
        let src = gradient(16, 16);
        // 振幅为 0:恒等(块宽钳位不参与判断)
        let mut none = src.clone();
        ripple_blocks(&mut none, 0.0, 0.0, 20.0, 20.0, 0.0, 0.0);
        assert_eq!(none.as_raw(), src.as_raw(), "两根振幅都是 0 时应恒等");
        // 只给横轴振幅:x 列为 0 的像素 sin(0)=0,不该动;其余横向位移
        let mut img = src.clone();
        ripple_blocks(&mut img, 3.0, 0.0, 20.0, 20.0, 0.0, 0.0);
        for y in 0..16 {
            assert_eq!(
                img.get_pixel(0, y).0,
                src.get_pixel(0, y).0,
                "x=0 处相位为 0,应原地不动"
            );
        }
        assert_ne!(img.as_raw(), src.as_raw(), "有振幅时应产生位移");
        assert_only_existing_colors(&img, &src);
    }

    #[test]
    fn ripple_blocks_clamps_the_block_width() {
        // 块宽小于 1 钳到 1,负块宽同样钳到 1 —— 两者结果必须一致
        let src = gradient(16, 16);
        let mut a = src.clone();
        let mut b = src.clone();
        ripple_blocks(&mut a, 2.0, 0.0, 0.2, 20.0, 0.0, 0.0);
        ripple_blocks(&mut b, 2.0, 0.0, -7.0, 20.0, 0.0, 0.0);
        assert_eq!(a.as_raw(), b.as_raw(), "越界块宽都应钳到 1");
        let mut wide = src.clone();
        ripple_blocks(&mut wide, 2.0, 0.0, 5000.0, 5000.0, 0.0, 0.0);
        assert_only_existing_colors(&wide, &src);
    }

    #[test]
    fn circle_splash_pulls_outside_onto_the_ring() {
        let src = gradient(16, 16);
        let mut img = src.clone();
        circle_splash(&mut img, 0.5, 0.5, 4.0);
        // 圆心(半径内)与圆周上的像素保持原样
        assert_eq!(img.get_pixel(8, 8).0, src.get_pixel(8, 8).0, "圆内应原样");
        assert_eq!(img.get_pixel(8, 12).0, src.get_pixel(8, 12).0, "圆周本身原样");
        // (8,0) 到圆心距离 8,同角方向圆周点是 (8,4)
        assert_eq!(
            img.get_pixel(8, 0).0,
            src.get_pixel(8, 4).0,
            "圆外应换成同角度的圆周像素"
        );
        assert_only_existing_colors(&img, &src);
    }

    #[test]
    fn circle_splash_out_of_range_radius_is_identity() {
        let src = gradient(16, 16);
        let mut img = src.clone();
        circle_splash(&mut img, 0.5, 0.5, 301.0);
        assert_eq!(img.as_raw(), src.as_raw(), "半径超过 300 应整幅恒等");
        let mut img = src.clone();
        circle_splash(&mut img, 0.5, 0.5, -1.0);
        assert_eq!(img.as_raw(), src.as_raw(), "半径为负应整幅恒等");
        let mut img = src.clone();
        circle_splash(&mut img, 1.5, 0.5, 4.0);
        assert_eq!(img.as_raw(), src.as_raw(), "圆心落在图外应整幅恒等");
    }

    #[test]
    fn crystallize_collapses_colours_without_inventing_any() {
        let src = gradient(32, 32);
        let mut img = src.clone();
        crystallize(&mut img, 8.0);
        let unique = |im: &RgbaImage| -> usize {
            let mut set = std::collections::HashSet::new();
            for p in im.pixels() {
                set.insert(p.0);
            }
            set.len()
        };
        assert!(
            unique(&img) * 2 < unique(&src),
            "晶格化应大幅减少色数,实得 {} → {}",
            unique(&src),
            unique(&img)
        );
        assert_only_existing_colors(&img, &src);
        // 格长出界:两侧都恒等
        for bad in [0.5, 301.0] {
            let mut im = src.clone();
            crystallize(&mut im, bad);
            assert_eq!(im.as_raw(), src.as_raw(), "格长 {bad} 出界应恒等");
        }
    }

    #[test]
    fn waterfall_collapses_one_side_onto_the_line_and_keeps_the_other() {
        let src = gradient(16, 16);
        let mut img = src.clone();
        waterfall(&mut img, 0.5, 0.5, 0.0);
        // 角度 <90:线上方的行全部塌到 (x, H/2)
        for x in 0..16 {
            assert_eq!(
                img.get_pixel(x, 3).0,
                src.get_pixel(x, 8).0,
                "上方行应塌到折叠线"
            );
        }
        // 线下方保持原样(没有镜像)
        assert_eq!(img.get_pixel(3, 12).0, src.get_pixel(3, 12).0, "另一侧应原样");
        assert_eq!(img.get_pixel(3, 8).0, src.get_pixel(3, 8).0, "折叠线本身不变");
    }

    #[test]
    fn waterfall_quadrants_pick_the_collapsing_axis() {
        let src = gradient(16, 16);
        // 90..180:线右侧的列塌到 (W/2, y)
        let mut img = src.clone();
        waterfall(&mut img, 0.5, 0.5, 120.0);
        assert_eq!(img.get_pixel(15, 2).0, src.get_pixel(8, 2).0, "右侧应塌到线上");
        assert_eq!(img.get_pixel(2, 15).0, src.get_pixel(2, 15).0, "左侧应原样");
        // 270:线左侧的列塌到 (W/2, y)
        let mut img = src.clone();
        waterfall(&mut img, 0.5, 0.5, 270.0);
        assert_eq!(img.get_pixel(0, 2).0, src.get_pixel(8, 2).0, "左侧应塌到线上");
        assert_eq!(img.get_pixel(15, 2).0, src.get_pixel(15, 2).0, "右侧应原样");
        // 出界恒等
        for bad in [-1.0, 271.0] {
            let mut im = src.clone();
            waterfall(&mut im, 0.5, 0.5, bad);
            assert_eq!(im.as_raw(), src.as_raw(), "角度 {bad} 出界应恒等");
        }
        assert_only_existing_colors(&img, &src);
    }

    #[test]
    fn hsl_filter_neutral_params_are_identity() {
        let src = gradient(16, 16);
        let mut img = src.clone();
        hsl_filter(&mut img, 0.0, 0.0, 0.0);
        assert_eq!(img.as_raw(), src.as_raw(), "三根轴都是 0 时应逐像素恒等");
    }

    #[test]
    fn hsl_filter_rotates_hue_by_the_signed_axis() {
        let mut img = RgbaImage::from_pixel(1, 1, Rgba([255, 0, 0, 128]));
        // 参考件是 H' = H − hue,故 hue=180 把红转成青
        hsl_filter(&mut img, 180.0, 0.0, 0.0);
        let p = img.get_pixel(0, 0).0;
        assert_eq!(p[0], 0, "红色转 180° 后红通道应归零,实得 {}", p[0]);
        assert_eq!((p[1], p[2]), (255, 255), "应得到青色,实得 {:?}", p);
        assert_eq!(p[3], 128, "透明度必须原样透传");
    }

    #[test]
    fn hsl_filter_saturates_and_lightens_on_the_documented_axes() {
        // sat = −100 → S' = 0 → 三通道同值(去色),且亮度不变
        let mut img = RgbaImage::from_pixel(1, 1, Rgba([200, 60, 20, 255]));
        hsl_filter(&mut img, 0.0, -100.0, 0.0);
        let p = img.get_pixel(0, 0).0;
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
        assert_eq!(p[0], 110, "去色后应等于亮度 (max+min)/2,实得 {}", p[0]);
        // lum = +100 → L' = L + 1 → 全部钳到 1 → 纯白
        let mut img = RgbaImage::from_pixel(1, 1, Rgba([200, 60, 20, 255]));
        hsl_filter(&mut img, 0.0, 0.0, 100.0);
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
        // 越界参数钳到边界:hue=9999 与 180 必须同结果
        let mut a = RgbaImage::from_pixel(1, 1, Rgba([10, 200, 90, 255]));
        let mut b = a.clone();
        hsl_filter(&mut a, 9999.0, 0.0, 0.0);
        hsl_filter(&mut b, 180.0, 0.0, 0.0);
        assert_eq!(a.as_raw(), b.as_raw(), "色相偏移应钳进 ±180");
    }
}
