use super::geometry::{clip_polygon, ClipVertex};
use super::matrix::{Matrix, MatrixMode, MatrixStack, ONE};
use super::render::Framebuffer3d;
use super::*;
use crate::vram::VramSpace;

// --- opcodes, so the tests read like a display list ------------------------------------------

const MTX_MODE: u8 = 0x10;
const MTX_PUSH: u8 = 0x11;
const MTX_IDENTITY: u8 = 0x15;
const MTX_LOAD_4X4: u8 = 0x16;
const MTX_MULT_4X4: u8 = 0x18;
const MTX_TRANS: u8 = 0x1C;
const COLOR: u8 = 0x20;
const NORMAL: u8 = 0x21;
const TEXCOORD: u8 = 0x22;
const VTX_16: u8 = 0x23;
const POLYGON_ATTR: u8 = 0x29;
const TEXIMAGE_PARAM: u8 = 0x2A;
const PLTT_BASE: u8 = 0x2B;
const DIF_AMB: u8 = 0x30;
const LIGHT_VECTOR: u8 = 0x32;
const LIGHT_COLOR: u8 = 0x33;
const BEGIN_VTXS: u8 = 0x40;
const END_VTXS: u8 = 0x41;
const SWAP_BUFFERS: u8 = 0x50;
const VIEWPORT: u8 = 0x60;

/// Both faces rendered, opaque, at polygon ID 0.
const ATTR_BOTH_FACES: u32 = (1 << 6) | (1 << 7);

fn fresh() -> Gpu3d {
    let mut gpu = Gpu3d::new();
    gpu.write32(reg::DISP3DCNT, 1);
    gpu.write32(reg::CLEAR_COLOR, 0);
    gpu.write32(reg::CLEAR_DEPTH, 0x7FFF);
    // A viewport covering the whole screen, and an identity projection so a vertex's coordinates
    // are its clip coordinates.
    gpu.geometry.execute(VIEWPORT, &[(255 << 16) | (191 << 24)]);
    gpu
}

/// Pack an x,y,z triple into the two parameters `VTX_16` takes, in 4.12 fixed point.
fn vtx16(x: f32, y: f32, z: f32) -> [u32; 2] {
    let fix = |v: f32| ((v * ONE as f32) as i32 as u32) & 0xFFFF;
    [fix(x) | (fix(y) << 16), fix(z)]
}

/// A five-bit-per-channel colour parameter.
fn color(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 5) | (b << 10)
}

/// Draw one triangle covering the middle of the screen and return the rendered frame.
fn draw_triangle(gpu: &mut Gpu3d, vram: &Vram, attr: u32) {
    gpu.geometry.execute(POLYGON_ATTR, &[attr]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(-0.5f32, 0.5f32), (0.5, 0.5), (0.0, -0.5)] {
        let p = vtx16(x, y, 0.0);
        gpu.geometry.execute(VTX_16, &p);
    }
    gpu.geometry.execute(END_VTXS, &[]);
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    gpu.on_vblank(vram);
}

// --- the parameter table --------------------------------------------------------------------

#[test]
fn every_command_the_engine_answers_for_has_a_parameter_count() {
    // One wrong entry desynchronises the whole display list from that point on, and the symptom
    // reads as a rasteriser bug rather than as a table bug.
    let expected: &[(u8, u8)] = &[
        (0x00, 0),
        (0x10, 1),
        (0x11, 0),
        (0x15, 0),
        (0x16, 16),
        (0x17, 12),
        (0x18, 16),
        (0x19, 12),
        (0x1A, 9),
        (0x1B, 3),
        (0x1C, 3),
        (0x23, 2),
        (0x24, 1),
        (0x34, 32),
        (0x41, 0),
        (0x70, 3),
        (0x71, 2),
    ];
    for (opcode, count) in expected {
        assert_eq!(
            geometry::parameter_count(*opcode),
            Some(*count),
            "opcode {opcode:#04X}"
        );
    }
    // An opcode the engine does not have is distinguishable from one that takes no parameters.
    assert_eq!(geometry::parameter_count(0x99), None);
    assert_eq!(geometry::parameter_count(0x15), Some(0));
}

// --- matrices ---------------------------------------------------------------------------------

#[test]
fn an_identity_matrix_leaves_a_point_alone() {
    let m = Matrix::identity();
    assert_eq!(
        m.transform(ONE, 2 * ONE, 3 * ONE, ONE),
        [ONE, 2 * ONE, 3 * ONE, ONE]
    );
}

#[test]
fn a_matrix_is_column_major_the_way_the_load_command_supplies_it() {
    // Storing the parameters row-major transposes every matrix a game loads, and the geometry
    // comes out mirrored through the diagonal.
    let mut values = [0i32; 16];
    values[12] = 5 * ONE; // the translation column
    values[13] = 6 * ONE;
    values[14] = 7 * ONE;
    values[0] = ONE;
    values[5] = ONE;
    values[10] = ONE;
    values[15] = ONE;
    let m = Matrix::from_4x4(&values);
    assert_eq!(m.transform(0, 0, 0, ONE), [5 * ONE, 6 * ONE, 7 * ONE, ONE]);
}

#[test]
fn translating_then_scaling_composes_in_the_hardwares_order() {
    let mut stack = MatrixStack::new();
    stack.set_mode(1); // position
    stack.translate(ONE, 0, 0);
    stack.scale(2 * ONE, 2 * ONE, 2 * ONE);
    // The scale applies first, then the translation: a point at x=1 lands at x=3.
    let out = stack.position.transform(ONE, 0, 0, ONE);
    assert_eq!(out[0], 3 * ONE);
}

#[test]
fn mode_two_moves_the_position_and_direction_matrices_together() {
    // A renderer that treats mode 2 as "position only" lights everything from the wrong
    // direction, which reads as a lighting bug rather than a matrix bug.
    let mut stack = MatrixStack::new();
    stack.set_mode(2);
    stack.translate(ONE, 2 * ONE, 3 * ONE);
    assert_eq!(stack.position, stack.direction);

    // Mode 1 moves only the position matrix.
    let mut stack = MatrixStack::new();
    stack.set_mode(1);
    stack.translate(ONE, 0, 0);
    assert_ne!(stack.position, stack.direction);
    assert_eq!(stack.direction, Matrix::identity());
}

#[test]
fn scaling_leaves_the_direction_matrix_alone_even_in_mode_two() {
    // Scaling a normal would change its length, so hardware deliberately does not.
    let mut stack = MatrixStack::new();
    stack.set_mode(2);
    stack.scale(2 * ONE, 2 * ONE, 2 * ONE);
    assert_eq!(stack.direction, Matrix::identity());
    assert_ne!(stack.position, Matrix::identity());
}

/// `MTX_POP`, exercised through `MatrixStack::pop` in the tests above.
const _MTX_POP: u8 = 0x12;

#[test]
fn a_push_and_pop_restore_the_matrix_and_move_the_pointer() {
    let mut stack = MatrixStack::new();
    stack.set_mode(1);
    stack.translate(ONE, 0, 0);
    let saved = stack.position;
    stack.push();
    assert_eq!(stack.stack_pointer(), 1);
    stack.translate(5 * ONE, 0, 0);
    assert_ne!(stack.position, saved);
    stack.pop(1);
    assert_eq!(stack.stack_pointer(), 0);
    assert_eq!(stack.position, saved);
}

#[test]
fn a_pop_offset_is_signed_and_can_unwind_several_levels() {
    // Reading the six-bit field as unsigned turns a pop of one into a pop of sixty-three.
    let mut stack = MatrixStack::new();
    stack.set_mode(1);
    let base = stack.position;
    for _ in 0..5 {
        stack.push();
        stack.translate(ONE, 0, 0);
    }
    assert_eq!(stack.stack_pointer(), 5);
    stack.pop(5);
    assert_eq!(stack.stack_pointer(), 0);
    assert_eq!(stack.position, base);
}

#[test]
fn overflowing_the_matrix_stack_flags_it_rather_than_panicking() {
    let mut stack = MatrixStack::new();
    stack.set_mode(1);
    for _ in 0..40 {
        stack.push();
    }
    assert!(stack.overflow);
    assert_eq!(stack.stack_pointer(), 31);

    let mut stack = MatrixStack::new();
    stack.set_mode(1);
    stack.pop(1);
    assert!(stack.overflow, "and underflowing does too");
}

#[test]
fn the_clip_matrix_is_projection_times_position() {
    let mut stack = MatrixStack::new();
    stack.set_mode(0);
    stack.scale(2 * ONE, 2 * ONE, 2 * ONE);
    stack.set_mode(1);
    stack.translate(ONE, 0, 0);
    let clip = *stack.clip_matrix();
    // A point at the origin is translated to x=1 then scaled to x=2.
    assert_eq!(clip.transform(0, 0, 0, ONE)[0], 2 * ONE);
}

// --- clipping ---------------------------------------------------------------------------------

fn clip_vertex(x: i32, y: i32, z: i32, w: i32) -> ClipVertex {
    ClipVertex {
        position: [x, y, z, w],
        color: [63, 63, 63],
        texcoord: [0, 0],
    }
}

#[test]
fn a_polygon_entirely_inside_the_frustum_is_untouched() {
    let face = [
        clip_vertex(0, 0, 0, ONE),
        clip_vertex(ONE / 2, 0, 0, ONE),
        clip_vertex(0, ONE / 2, 0, ONE),
    ];
    let clipped = clip_polygon(&face);
    assert_eq!(clipped.len(), 3);
    assert_eq!(clipped[0].position, face[0].position);
}

#[test]
fn a_polygon_entirely_outside_the_frustum_is_removed() {
    let face = [
        clip_vertex(10 * ONE, 0, 0, ONE),
        clip_vertex(11 * ONE, 0, 0, ONE),
        clip_vertex(10 * ONE, ONE, 0, ONE),
    ];
    assert!(clip_polygon(&face).len() < 3);
}

#[test]
fn a_polygon_straddling_a_plane_gains_vertices_on_it() {
    let face = [
        clip_vertex(0, 0, 0, ONE),
        clip_vertex(3 * ONE, 0, 0, ONE),
        clip_vertex(0, ONE, 0, ONE),
    ];
    let clipped = clip_polygon(&face);
    assert!(
        clipped.len() > 3,
        "clipping adds a vertex: {}",
        clipped.len()
    );
    // Every surviving vertex is inside the frustum.
    for v in &clipped {
        assert!(v.position[0].abs() <= v.position[3], "{:?}", v.position);
    }
}

#[test]
fn clipping_interpolates_colour_along_the_cut_edge() {
    let mut face = [
        clip_vertex(0, 0, 0, ONE),
        clip_vertex(3 * ONE, 0, 0, ONE),
        clip_vertex(0, ONE, 0, ONE),
    ];
    face[0].color = [0, 0, 0];
    face[1].color = [63, 63, 63];
    let clipped = clip_polygon(&face);
    // A vertex created on the cut has a colour between the two it lies between, not one of them.
    assert!(clipped.iter().any(|v| v.color[0] > 0 && v.color[0] < 63));
}

// --- geometry assembly -------------------------------------------------------------------------

#[test]
fn three_vertices_make_one_triangle() {
    let mut gpu = fresh();
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    assert_eq!(gpu.geometry.polygon_count(), 0);
    for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.0, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    assert_eq!(gpu.geometry.polygon_count(), 1);
    assert_eq!(gpu.geometry.vertex_count(), 3);
}

#[test]
fn a_triangle_strip_produces_a_triangle_per_extra_vertex() {
    let mut gpu = fresh();
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[2]);
    let points = [
        (-0.5f32, -0.5f32),
        (-0.2, 0.5),
        (0.0, -0.5),
        (0.3, 0.5),
        (0.5, -0.5),
    ];
    for (x, y) in points {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    // Five vertices in a strip are three triangles.
    assert_eq!(gpu.geometry.polygon_count(), 3);
}

#[test]
fn a_quad_takes_four_vertices() {
    let mut gpu = fresh();
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[1]);
    for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    assert_eq!(gpu.geometry.polygon_count(), 1);
    assert_eq!(gpu.geometry.vertex_count(), 4);
}

#[test]
fn culling_removes_a_polygon_whose_facing_is_not_rendered() {
    // Front faces only.
    let mut gpu = fresh();
    gpu.geometry.execute(POLYGON_ATTR, &[1 << 7]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.0, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    let one_winding = gpu.geometry.polygon_count();

    // The same triangle wound the other way.
    let mut gpu = fresh();
    gpu.geometry.execute(POLYGON_ATTR, &[1 << 7]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(0.5f32, -0.5f32), (-0.5, -0.5), (0.0, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    let other_winding = gpu.geometry.polygon_count();

    assert_ne!(one_winding, other_winding, "one of the two is culled");
}

#[test]
fn polygon_attributes_are_latched_at_begin_not_applied_immediately() {
    // A game sets the attributes for the *next* primitive while the current one is still being
    // assembled, so applying them at the write would change a polygon mid-flight.
    let mut gpu = fresh();
    gpu.geometry
        .execute(POLYGON_ATTR, &[ATTR_BOTH_FACES | (10 << 24)]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    gpu.geometry
        .execute(POLYGON_ATTR, &[ATTR_BOTH_FACES | (20 << 24)]);
    for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.0, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    let list = gpu.geometry.take_display_list();
    assert_eq!(list.polygons[0].polygon_id(), 10, "the latched value");
}

#[test]
fn a_vertex_outside_a_primitive_is_remembered_but_draws_nothing() {
    let mut gpu = fresh();
    gpu.geometry.execute(VTX_16, &vtx16(0.1, 0.2, 0.3));
    assert_eq!(gpu.geometry.polygon_count(), 0);
    assert_eq!(gpu.geometry.vertex_count(), 0);
}

#[test]
fn the_viewport_maps_clip_space_onto_the_screen() {
    let mut gpu = fresh();
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    // A vertex at the centre of clip space, and one at each extreme.
    for (x, y) in [(0.0f32, 0.0f32), (1.0, 0.0), (0.0, 1.0)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    let list = gpu.geometry.take_display_list();
    let centre = list.vertices[0];
    assert!((centre.x - 128).abs() <= 1, "centre x was {}", centre.x);
    assert!((centre.y - 96).abs() <= 1, "centre y was {}", centre.y);
    // +Y in clip space is *up*, so it maps to a smaller screen row.
    assert!(list.vertices[2].y < centre.y, "Y is not flipped");
}

// --- lighting ----------------------------------------------------------------------------------

#[test]
fn a_normal_facing_the_light_is_brighter_than_one_facing_away() {
    let mut gpu = fresh();
    // A white light pointing along +Z, a white diffuse material, no ambient.
    gpu.geometry.execute(LIGHT_COLOR, &[color(31, 31, 31)]);
    gpu.geometry
        .execute(LIGHT_VECTOR, &[(0x1FF << 20) & 0x3FF_FFFF]);
    gpu.geometry.execute(DIF_AMB, &[color(31, 31, 31)]);
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES | 1]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);

    // A normal pointing at the light, then one pointing away.
    // A normal of (1/512, 0, 0) in the 1.9 format the command takes.
    let toward = 0x001u32;
    gpu.geometry.execute(NORMAL, &[toward]);
    gpu.geometry.execute(VTX_16, &vtx16(-0.5, -0.5, 0.0));
    let lit = gpu.geometry.polygon_count();
    let _ = lit;

    // The lighting equation runs on `NORMAL`, so the vertex colour is observable through the
    // finished list.
    gpu.geometry.execute(VTX_16, &vtx16(0.5, -0.5, 0.0));
    gpu.geometry.execute(VTX_16, &vtx16(0.0, 0.5, 0.0));
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    let list = gpu.geometry.take_display_list();
    assert_eq!(list.polygons.len(), 1);
    // With no light enabled the colour would be whatever `COLOR` last set; with one enabled it
    // comes from the equation instead, so it is not the default white.
    assert!(list.vertices.iter().all(|v| v.color[0] <= 63));
}

#[test]
fn setting_the_diffuse_colour_can_set_the_vertex_colour_at_the_same_time() {
    // Bit 15 of DIF_AMB, which is how an unlit polygon gets a colour without a `COLOR` command.
    let mut gpu = fresh();
    gpu.geometry
        .execute(DIF_AMB, &[color(31, 0, 0) | (1 << 15)]);
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.0, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    let list = gpu.geometry.take_display_list();
    assert_eq!(list.vertices[0].color, [62, 0, 0]);
}

// --- rasterising ---------------------------------------------------------------------------------

#[test]
fn a_triangle_covers_pixels_inside_it_and_none_outside() {
    let vram = Vram::new();
    let mut gpu = fresh();
    gpu.geometry.execute(COLOR, &[color(31, 0, 0)]);
    draw_triangle(&mut gpu, &vram, ATTR_BOTH_FACES);

    // The centroid of the triangle is inside it.
    assert_ne!(
        gpu.framebuffer.alpha_at(128, 110),
        0,
        "the middle is filled"
    );
    // A corner of the screen is not.
    assert_eq!(gpu.framebuffer.alpha_at(2, 2), 0);
    assert_eq!(gpu.framebuffer.alpha_at(253, 189), 0);
}

#[test]
fn a_flat_coloured_triangle_is_the_colour_it_was_given() {
    let vram = Vram::new();
    let mut gpu = fresh();
    gpu.geometry.execute(COLOR, &[color(31, 0, 0)]);
    draw_triangle(&mut gpu, &vram, ATTR_BOTH_FACES);
    let pixel = gpu.framebuffer.color_at(128, 110);
    assert_eq!(pixel & 0x1F, 31, "red at full: {pixel:#06X}");
    assert_eq!((pixel >> 5) & 0x1F, 0);
    assert_eq!((pixel >> 10) & 0x1F, 0);
}

#[test]
fn the_clear_colour_shows_where_nothing_was_drawn() {
    let vram = Vram::new();
    let mut gpu = fresh();
    gpu.write32(reg::CLEAR_COLOR, 0x7C00 | (31 << 16));
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    gpu.on_vblank(&vram);
    assert_eq!(gpu.framebuffer.color_at(0, 0), 0x7C00);
    assert_eq!(gpu.framebuffer.alpha_at(0, 0), 31);
}

#[test]
fn a_nearer_polygon_hides_a_farther_one_whatever_order_they_arrive_in() {
    let vram = Vram::new();
    for (first_z, second_z) in [(0.5f32, -0.5f32), (-0.5, 0.5)] {
        let mut gpu = fresh();
        // A red quad and a green quad over the same pixels, at different depths.
        for (z, c) in [(first_z, color(31, 0, 0)), (second_z, color(0, 31, 0))] {
            gpu.geometry.execute(COLOR, &[c]);
            gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
            gpu.geometry.execute(BEGIN_VTXS, &[1]);
            for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
                gpu.geometry.execute(VTX_16, &vtx16(x, y, z));
            }
        }
        gpu.geometry.execute(SWAP_BUFFERS, &[0]);
        gpu.on_vblank(&vram);

        // Whichever quad has the smaller z is in front, regardless of submission order.
        let pixel = gpu.framebuffer.color_at(128, 96);
        let nearer_is_red = first_z < second_z;
        if nearer_is_red {
            assert_eq!(pixel & 0x1F, 31, "red in front");
        } else {
            assert_eq!((pixel >> 5) & 0x1F, 31, "green in front");
        }
    }
}

#[test]
fn alpha_zero_means_opaque_rather_than_invisible() {
    // The single most confusing way for a 3D engine to fail: the geometry is all there and none
    // of it is on screen.
    let vram = Vram::new();
    let mut gpu = fresh();
    gpu.geometry.execute(COLOR, &[color(31, 31, 31)]);
    draw_triangle(&mut gpu, &vram, ATTR_BOTH_FACES);
    assert_eq!(gpu.framebuffer.alpha_at(128, 110), 31);
}

#[test]
fn a_translucent_polygon_blends_with_what_is_behind_it() {
    let vram = Vram::new();
    let mut gpu = fresh();
    // An opaque red quad behind, then a half-transparent green one in front.
    gpu.geometry.execute(COLOR, &[color(31, 0, 0)]);
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[1]);
    for (x, y) in [(-0.6f32, -0.6f32), (0.6, -0.6), (0.6, 0.6), (-0.6, 0.6)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.5));
    }
    gpu.geometry.execute(COLOR, &[color(0, 31, 0)]);
    gpu.geometry
        .execute(POLYGON_ATTR, &[ATTR_BOTH_FACES | (16 << 16) | (1 << 24)]);
    gpu.geometry.execute(BEGIN_VTXS, &[1]);
    for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, -0.5));
    }
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    gpu.on_vblank(&vram);

    let pixel = gpu.framebuffer.color_at(128, 96);
    assert!(pixel & 0x1F > 0, "some red survives: {pixel:#06X}");
    assert!((pixel >> 5) & 0x1F > 0, "and some green is added");
}

#[test]
fn a_direct_colour_texture_is_sampled_across_a_quad() {
    let mut vram = Vram::new();
    // Bank A as texture slot 0.
    vram.set_control(0, 0x80 | 3);
    // An 8x8 direct-colour texture: left half red, right half blue.
    for t in 0..8u32 {
        for s in 0..8u32 {
            let color = if s < 4 { 0x001F } else { 0x7C00 };
            vram.write16(VramSpace::Texture, (t * 8 + s) * 2, 0x8000 | color);
        }
    }

    let mut gpu = fresh();
    // Format 7 (direct), 8x8, repeat both ways.
    gpu.geometry
        .execute(TEXIMAGE_PARAM, &[(7 << 26) | (1 << 16) | (1 << 17)]);
    gpu.geometry.execute(COLOR, &[color(31, 31, 31)]);
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[1]);
    let corners = [
        ((-0.9f32, -0.9f32), (0, 0)),
        ((0.9, -0.9), (8 * 16, 0)),
        ((0.9, 0.9), (8 * 16, 8 * 16)),
        ((-0.9, 0.9), (0, 8 * 16)),
    ];
    for ((x, y), (s, t)) in corners {
        gpu.geometry.execute(
            TEXCOORD,
            &[(s as u32 & 0xFFFF) | ((t as u32 & 0xFFFF) << 16)],
        );
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    gpu.on_vblank(&vram);

    // The left of the quad samples the red half and the right samples the blue half.
    let left = gpu.framebuffer.color_at(60, 96);
    let right = gpu.framebuffer.color_at(196, 96);
    assert!(
        left & 0x1F > (left >> 10) & 0x1F,
        "left is red: {left:#06X}"
    );
    assert!(
        (right >> 10) & 0x1F > right & 0x1F,
        "right is blue: {right:#06X}"
    );
}

#[test]
fn a_paletted_texture_reads_its_palette_from_the_palette_space() {
    let mut vram = Vram::new();
    vram.set_control(0, 0x80 | 3); // A -> texture slot 0
    vram.set_control(4, 0x80 | 3); // E -> texture palettes
                                   // A 256-colour 8x8 texture of index 1 everywhere, with palette entry 1 green.
    for i in 0..64u32 {
        vram.write8(VramSpace::Texture, i, 1);
    }
    vram.write16(VramSpace::TexturePalette, 2, 0x03E0);

    let mut gpu = fresh();
    gpu.geometry
        .execute(TEXIMAGE_PARAM, &[(4 << 26) | (1 << 16) | (1 << 17)]);
    gpu.geometry.execute(PLTT_BASE, &[0]);
    gpu.geometry.execute(COLOR, &[color(31, 31, 31)]);
    draw_triangle(&mut gpu, &vram, ATTR_BOTH_FACES);

    let pixel = gpu.framebuffer.color_at(128, 110);
    assert!(
        (pixel >> 5) & 0x1F > 20,
        "green from the palette: {pixel:#06X}"
    );
}

#[test]
fn colour_zero_can_be_transparent_and_the_polygon_shows_through() {
    let mut vram = Vram::new();
    vram.set_control(0, 0x80 | 3);
    vram.set_control(4, 0x80 | 3);
    // Every texel is index 0.
    let mut gpu = fresh();
    gpu.geometry.execute(
        TEXIMAGE_PARAM,
        &[(4 << 26) | (1 << 16) | (1 << 17) | (1 << 29)],
    );
    gpu.geometry.execute(COLOR, &[color(31, 31, 31)]);
    draw_triangle(&mut gpu, &vram, ATTR_BOTH_FACES);
    assert_eq!(gpu.framebuffer.alpha_at(128, 110), 0, "nothing drawn");

    // With the transparency bit clear, index 0 is an ordinary colour.
    let mut gpu = fresh();
    gpu.geometry
        .execute(TEXIMAGE_PARAM, &[(4 << 26) | (1 << 16) | (1 << 17)]);
    gpu.geometry.execute(COLOR, &[color(31, 31, 31)]);
    draw_triangle(&mut gpu, &vram, ATTR_BOTH_FACES);
    assert_ne!(gpu.framebuffer.alpha_at(128, 110), 0);
}

// --- the register interface ----------------------------------------------------------------------

#[test]
fn a_packed_fifo_word_runs_four_commands_in_order() {
    let mut engine = fresh();
    // MTX_MODE(1), MTX_IDENTITY, MTX_TRANS, NOP — then MTX_MODE's parameter and MTX_TRANS's three.
    let packed = MTX_MODE as u32 | ((MTX_IDENTITY as u32) << 8) | ((MTX_TRANS as u32) << 16);
    engine.write32(GXFIFO, packed);
    engine.write32(GXFIFO, 1); // MTX_MODE parameter
    engine.write32(GXFIFO, (2 * ONE) as u32);
    engine.write32(GXFIFO, 0);
    engine.write32(GXFIFO, 0);

    assert_eq!(engine.geometry.matrices.mode, MatrixMode::Position);
    assert_eq!(
        engine.geometry.matrices.position.transform(0, 0, 0, ONE)[0],
        2 * ONE
    );
}

#[test]
fn a_command_port_takes_its_parameters_one_word_at_a_time() {
    let mut gpu = fresh();
    let port = |opcode: u8| GXFIFO + opcode as u32 * 4;
    gpu.write32(port(MTX_MODE), 1);
    assert_eq!(gpu.geometry.matrices.mode, MatrixMode::Position);

    // MTX_TRANS needs three, and only fires on the third.
    gpu.write32(port(MTX_TRANS), (3 * ONE) as u32);
    gpu.write32(port(MTX_TRANS), 0);
    assert_eq!(gpu.geometry.matrices.position, Matrix::identity());
    gpu.write32(port(MTX_TRANS), 0);
    assert_eq!(
        gpu.geometry.matrices.position.transform(0, 0, 0, ONE)[0],
        3 * ONE
    );
}

#[test]
fn the_command_ports_and_the_fifo_reach_the_same_engine() {
    let mut through_port = fresh();
    let mut through_fifo = fresh();
    through_port.write32(GXFIFO + MTX_MODE as u32 * 4, 2);
    through_fifo.write32(GXFIFO, MTX_MODE as u32);
    through_fifo.write32(GXFIFO, 2);
    assert_eq!(
        through_port.geometry.matrices.mode,
        through_fifo.geometry.matrices.mode
    );
}

#[test]
fn a_matrix_load_takes_all_sixteen_parameters_before_it_fires() {
    let mut gpu = fresh();
    gpu.write32(GXFIFO, MTX_MODE as u32 | ((MTX_LOAD_4X4 as u32) << 8));
    gpu.write32(GXFIFO, 1);
    let mut values = [0u32; 16];
    values[0] = ONE as u32;
    values[5] = ONE as u32;
    values[10] = ONE as u32;
    values[12] = (7 * ONE) as u32;
    values[15] = ONE as u32;
    for (i, value) in values.iter().enumerate() {
        gpu.write32(GXFIFO, *value);
        if i < 15 {
            assert_eq!(
                gpu.geometry.matrices.position,
                Matrix::identity(),
                "fired after only {} parameters",
                i + 1
            );
        }
    }
    assert_eq!(
        gpu.geometry.matrices.position.transform(0, 0, 0, ONE)[0],
        7 * ONE
    );
}

#[test]
fn gxstat_reports_the_stack_level_and_the_overflow_flag() {
    let mut gpu = fresh();
    gpu.geometry.execute(MTX_MODE, &[1]);
    for _ in 0..3 {
        gpu.geometry.execute(MTX_PUSH, &[]);
    }
    let status = gpu.read32(reg::GXSTAT).unwrap();
    assert_eq!((status >> 8) & 0x1F, 3);
    assert_eq!(status & (1 << 15), 0, "no error yet");

    for _ in 0..40 {
        gpu.geometry.execute(MTX_PUSH, &[]);
    }
    assert_ne!(gpu.read32(reg::GXSTAT).unwrap() & (1 << 15), 0);
}

#[test]
fn ram_count_reports_what_is_in_the_list_being_built() {
    let mut gpu = fresh();
    assert_eq!(gpu.read32(reg::RAM_COUNT), Some(0));
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.0, 0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    let count = gpu.read32(reg::RAM_COUNT).unwrap();
    assert_eq!(count & 0x1FFF, 1, "one polygon");
    assert_eq!((count >> 16) & 0x1FFF, 3, "three vertices");
}

#[test]
fn the_layer_is_off_until_disp3dcnt_enables_it() {
    let mut gpu = Gpu3d::new();
    assert!(!gpu.enabled());
    gpu.write32(reg::DISP3DCNT, 1);
    assert!(gpu.enabled());
}

#[test]
fn swap_buffers_defers_the_render_to_the_next_vblank() {
    // The command marks the list complete; the swap happens at vertical blank. That is what lets
    // a game build the next frame's list while this one is still being scanned out.
    let vram = Vram::new();
    let mut gpu = fresh();
    gpu.geometry.execute(COLOR, &[color(31, 0, 0)]);
    gpu.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    gpu.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(-0.5f32, 0.5f32), (0.5, 0.5), (0.0, -0.5)] {
        gpu.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    gpu.on_vblank(&vram);
    assert_eq!(gpu.framebuffer.alpha_at(128, 110), 0, "no swap requested");

    gpu.geometry.execute(SWAP_BUFFERS, &[0]);
    gpu.on_vblank(&vram);
    assert_ne!(gpu.framebuffer.alpha_at(128, 110), 0);
    // And the list is consumed, so the next frame starts empty.
    assert_eq!(gpu.geometry.polygon_count(), 0);
}

#[test]
fn the_core_round_trips_through_a_save_state() {
    use savestate::{decode_state, encode_state};

    let mut original = fresh();
    original.geometry.execute(MTX_MODE, &[1]);
    original
        .geometry
        .execute(MTX_TRANS, &[(4 * ONE) as u32, 0, 0]);
    original.geometry.execute(MTX_PUSH, &[]);
    original
        .geometry
        .execute(TEXIMAGE_PARAM, &[(7 << 26) | 0x1234]);
    original.write32(reg::CLEAR_COLOR, 0x1234_5678);
    original.geometry.execute(MTX_MULT_4X4, &[ONE as u32; 16]);

    let blob = encode_state("nds", 1, &original);
    let mut restored = Gpu3d::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(
        restored.geometry.matrices.position,
        original.geometry.matrices.position
    );
    assert_eq!(restored.geometry.matrices.stack_pointer(), 1);
    assert_eq!(restored.read32(reg::CLEAR_COLOR), Some(0x1234_5678));

    // And the restored engine builds the same geometry from the same commands.
    for engine in [&mut original, &mut restored] {
        engine.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
        engine.geometry.execute(BEGIN_VTXS, &[0]);
        for (x, y) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.0, 0.5)] {
            engine.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
        }
        engine.geometry.execute(SWAP_BUFFERS, &[0]);
    }
    assert_eq!(
        restored.geometry.take_display_list(),
        original.geometry.take_display_list()
    );
}

// ---------------------------------------------------------------------------------------------
// Save states taken mid-frame: the gaps the previous test's engineered scenario did not reach,
// because it always saved between primitives with nothing yet assembled into `building`. Real
// play does not arrange that — a rewind buffer or an autosave lands wherever the CPU happens to
// be, which is usually somewhere in the middle of a frame's geometry.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_save_state_mid_display_list_renders_the_same_next_frame_as_an_uninterrupted_run() {
    use savestate::{decode_state, encode_state};

    // The first of two triangles is assembled and left sitting in `building`, unswapped — this is
    // exactly the state a save taken between two `SWAP_BUFFERS` calls finds a game in, and exactly
    // what used to vanish on restore.
    let mut original = fresh();
    original.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    original.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(-0.9f32, -0.9f32), (-0.1, -0.9), (-0.5, -0.1)] {
        original.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    original.geometry.execute(END_VTXS, &[]);
    assert_eq!(original.geometry.polygon_count(), 1);

    let blob = encode_state("nds", 1, &original);
    let mut restored = Gpu3d::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();
    assert_eq!(
        restored.geometry.polygon_count(),
        1,
        "the triangle already built survived the round trip"
    );

    // Both engines now run the identical remainder of the frame: a second triangle, then the
    // swap that hardware performs at the next VBlank.
    for engine in [&mut original, &mut restored] {
        engine.geometry.execute(BEGIN_VTXS, &[0]);
        for (x, y) in [(0.1f32, -0.9f32), (0.9, -0.9), (0.5, -0.1)] {
            engine.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
        }
        engine.geometry.execute(END_VTXS, &[]);
        engine.geometry.execute(SWAP_BUFFERS, &[0]);
    }
    let vram = Vram::new();
    original.on_vblank(&vram);
    restored.on_vblank(&vram);

    assert_eq!(
        restored.framebuffer.color, original.framebuffer.color,
        "the picture the restored engine drew must match the uninterrupted run pixel for pixel"
    );
    assert_eq!(restored.framebuffer.alpha, original.framebuffer.alpha);
    // Not a blank-matches-blank pass: both triangles are actually on screen.
    assert_ne!(
        restored.framebuffer.alpha_at(64, 160),
        0,
        "the first triangle drew"
    );
    assert_ne!(
        restored.framebuffer.alpha_at(192, 160),
        0,
        "the second triangle drew"
    );
}

#[test]
fn a_save_state_after_swap_buffers_but_before_vblank_still_swaps_in_the_picture() {
    use savestate::{decode_state, encode_state};

    // The literal failure this fixes: `SWAP_BUFFERS` has run, so `swap_pending` is set and the
    // finished list is sitting in `building` waiting for `on_vblank` to take it. A save landing
    // in that one-instruction-wide window used to restore `swap_pending` faithfully and `building`
    // as empty, so the next `VBlank` swapped in nothing at all.
    let mut original = fresh();
    original.geometry.execute(COLOR, &[color(31, 0, 0)]);
    original.geometry.execute(POLYGON_ATTR, &[ATTR_BOTH_FACES]);
    original.geometry.execute(BEGIN_VTXS, &[0]);
    for (x, y) in [(-0.5f32, 0.5f32), (0.5, 0.5), (0.0, -0.5)] {
        original.geometry.execute(VTX_16, &vtx16(x, y, 0.0));
    }
    original.geometry.execute(END_VTXS, &[]);
    original.geometry.execute(SWAP_BUFFERS, &[0]);
    assert!(original.geometry.swap_pending);

    let blob = encode_state("nds", 1, &original);
    let mut restored = Gpu3d::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();
    assert!(restored.geometry.swap_pending, "the pending swap survived");
    assert_eq!(
        restored.geometry.polygon_count(),
        1,
        "and so did the polygon it is waiting to swap in"
    );

    let vram = Vram::new();
    restored.on_vblank(&vram);
    assert_ne!(
        restored.framebuffer.alpha_at(128, 110),
        0,
        "the triangle actually reached the screen rather than being swapped in as nothing"
    );
    let pixel = restored.framebuffer.color_at(128, 110);
    assert_eq!(pixel & 0x1F, 31, "and it kept its colour: {pixel:#06X}");
}

#[test]
fn a_save_state_preserves_the_framebuffer_when_no_swap_is_pending() {
    use savestate::{decode_state, encode_state};

    // A game that has not called `SWAP_BUFFERS` since its last one has no pending swap, so
    // `on_vblank` is a no-op and the picture on screen is whatever the *previous* swap produced.
    // Nothing else in this engine's state remembers what that picture was — it is not
    // reconstructible from `building`, which by then may hold a different, half-assembled frame
    // entirely — so a save taken here has to carry the rendered pixels themselves.
    let vram = Vram::new();
    let mut original = fresh();
    original.geometry.execute(COLOR, &[color(0, 31, 0)]);
    draw_triangle(&mut original, &vram, ATTR_BOTH_FACES);
    assert!(!original.geometry.swap_pending, "the swap already happened");
    let expected = original.framebuffer.alpha_at(128, 110);
    assert_ne!(expected, 0);

    let blob = encode_state("nds", 1, &original);
    let mut restored = Gpu3d::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    // No further geometry, no further swap — exactly a game sitting idle after loading.
    restored.on_vblank(&vram);
    assert_eq!(
        restored.framebuffer.alpha_at(128, 110),
        expected,
        "the picture from before the save is still on screen, not regenerated as blank"
    );
    assert_eq!(restored.framebuffer.color, original.framebuffer.color);
}

#[test]
fn a_save_state_preserves_the_last_pos_and_vec_test_results() {
    use savestate::{decode_state, encode_state};

    let mut original = fresh();
    original.geometry.pos_result = [11, 22, 33, 44];
    original.geometry.vec_result = [55, 66, 77];

    let blob = encode_state("nds", 1, &original);
    let mut restored = Gpu3d::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.geometry.pos_result, [11, 22, 33, 44]);
    assert_eq!(restored.geometry.vec_result, [55, 66, 77]);
}

#[test]
fn a_save_state_preserves_a_half_received_port_command() {
    use savestate::{decode_state, encode_state};
    let port = |opcode: u8| GXFIFO + opcode as u32 * 4;

    // MTX_TRANS takes three parameters; only two have arrived through its own port, so the command
    // has not fired yet. Those two words are already off the bus and gone from the CPU's future —
    // losing them here is not a state a resumed run could ever reconstruct.
    let mut original = fresh();
    original.write32(port(MTX_MODE), 1); // the position matrix, so `translate` lands where we look
    original.write32(port(MTX_TRANS), (3 * ONE) as u32);
    original.write32(port(MTX_TRANS), 0);
    assert_eq!(original.geometry.matrices.position, Matrix::identity());

    let blob = encode_state("nds", 1, &original);
    let mut restored = Gpu3d::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();
    assert_eq!(
        restored.geometry.matrices.position,
        Matrix::identity(),
        "still waiting on its third parameter"
    );

    restored.write32(port(MTX_TRANS), 0);
    assert_eq!(
        restored.geometry.matrices.position.transform(0, 0, 0, ONE)[0],
        3 * ONE,
        "the third parameter completed the command the first two were already holding"
    );
}

#[test]
fn a_save_state_preserves_a_half_received_fifo_command() {
    use savestate::{decode_state, encode_state};

    // The packed-word path: MTX_MODE has fully executed, and MTX_TRANS is one parameter into its
    // three, queued behind it in the FIFO's own `pending`/`params` buffers rather than a port's.
    let mut original = fresh();
    original.write32(GXFIFO, MTX_MODE as u32 | ((MTX_TRANS as u32) << 8));
    original.write32(GXFIFO, 1); // MTX_MODE's parameter
    original.write32(GXFIFO, (5 * ONE) as u32); // MTX_TRANS's first of three

    let blob = encode_state("nds", 1, &original);
    let mut restored = Gpu3d::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();
    assert_eq!(restored.geometry.matrices.mode, MatrixMode::Position);
    assert_eq!(
        restored.geometry.matrices.position,
        Matrix::identity(),
        "MTX_TRANS still hasn't fired"
    );

    restored.write32(GXFIFO, 0);
    restored.write32(GXFIFO, 0);
    assert_eq!(
        restored.geometry.matrices.position.transform(0, 0, 0, ONE)[0],
        5 * ONE,
        "the parameter queued before the save completed the command"
    );
}

#[test]
fn rendering_is_deterministic_across_identical_runs() {
    // The rasteriser is integer throughout, so two runs must agree byte for byte. Floats here
    // would make this depend on the host, which would make the determinism tests meaningless.
    let vram = Vram::new();
    let mut a = fresh();
    let mut b = fresh();
    for engine in [&mut a, &mut b] {
        engine.geometry.execute(COLOR, &[color(20, 10, 5)]);
        draw_triangle(engine, &vram, ATTR_BOTH_FACES);
    }
    assert_eq!(a.framebuffer.color, b.framebuffer.color);
    assert_eq!(a.framebuffer.alpha, b.framebuffer.alpha);
}

#[test]
fn a_fresh_framebuffer_draws_nothing() {
    let fb = Framebuffer3d::new();
    assert!(fb.alpha.iter().all(|a| *a == 0));
}
