//#line 1
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// Matches BorderCornerSide enum in border.rs
#define SIDE_BOTH       0
#define SIDE_FIRST      1
#define SIDE_SECOND     2

vec2 get_radii(vec2 radius, vec2 invalid) {
    if (all(greaterThan(radius, vec2(0.0, 0.0)))) {
        return radius;
    }

    return invalid;
}

void set_radii(int style,
               vec2 radii,
               vec2 widths,
               vec2 adjusted_widths
#ifdef WR_DX11
               , out vec4 vRadii0
               , out vec4 vRadii1
#endif
               ) {
    vRadii0.xy = get_radii(radii, 2.0 * widths);
    vRadii0.zw = get_radii(radii - widths, -widths);

    switch (style) {
        case BORDER_STYLE_RIDGE:
        case BORDER_STYLE_GROOVE:
            vRadii1.xy = radii - adjusted_widths;
            // See comment in default branch
            vRadii1.zw = vec2(-100.0, -100.0);
            break;
        case BORDER_STYLE_DOUBLE:
            vRadii1.xy = get_radii(radii - adjusted_widths, -widths);
            vRadii1.zw = get_radii(radii - widths + adjusted_widths, -widths);
            break;
        default:
            // These aren't needed, so we set them to some reasonably large
            // negative value so later computations will discard them. This
            // avoids branches and numerical issues in the fragment shader.
            vRadii1.xy = vec2(-100.0, -100.0);
            vRadii1.zw = vec2(-100.0, -100.0);
            break;
    }
}


void set_edge_line(vec2 border_width,
                   vec2 outer_corner,
                   vec2 gradient_sign
#ifdef WR_DX11
                   , out vec4 vColorEdgeLine
#endif
                   ) {
    vec2 gradient = border_width * gradient_sign;
    vColorEdgeLine = vec4(outer_corner, vec2(-gradient.y, gradient.x));
}

void write_color(vec4 color0,
                 vec4 color1,
                 int style,
                 vec2 delta,
                 int instance_kind
#ifdef WR_DX11
                 , out vec4 vColor00
                 , out vec4 vColor01
                 , out vec4 vColor10
                 , out vec4 vColor11
#endif
                 ) {
    vec4 modulate;

    switch (style) {
        case BORDER_STYLE_GROOVE:
            modulate = vec4(1.0 - 0.3 * delta.x,
                            1.0 + 0.3 * delta.x,
                            1.0 - 0.3 * delta.y,
                            1.0 + 0.3 * delta.y);

            break;
        case BORDER_STYLE_RIDGE:
            modulate = vec4(1.0 + 0.3 * delta.x,
                            1.0 - 0.3 * delta.x,
                            1.0 + 0.3 * delta.y,
                            1.0 - 0.3 * delta.y);
            break;
        default:
            modulate = vec4(1.0, 1.0, 1.0, 1.0);
            break;
    }

    // Optionally mask out one side of the border corner,
    // depending on the instance kind.
    switch (instance_kind) {
        case SIDE_FIRST:
            color0.a = 0.0;
            break;
        case SIDE_SECOND:
            color1.a = 0.0;
            break;
    }

    vColor00 = vec4(color0.rgb * modulate.x, color0.a);
    vColor01 = vec4(color0.rgb * modulate.y, color0.a);
    vColor10 = vec4(color1.rgb * modulate.z, color1.a);
    vColor11 = vec4(color1.rgb * modulate.w, color1.a);
}

int select_style(int color_select, vec2 fstyle) {
    ivec2 style = ivec2(fstyle);

    switch (color_select) {
        case SIDE_BOTH:
        {
            // TODO(gw): A temporary hack! While we don't support
            //           border corners that have dots or dashes
            //           with another style, pretend they are solid
            //           border corners.
            bool has_dots = style.x == BORDER_STYLE_DOTTED ||
                            style.y == BORDER_STYLE_DOTTED;
            bool has_dashes = style.x == BORDER_STYLE_DASHED ||
                              style.y == BORDER_STYLE_DASHED;
            if (style.x != style.y && (has_dots || has_dashes))
                return BORDER_STYLE_SOLID;
            return style.x;
        }
        case SIDE_FIRST:
            return style.x;
        case SIDE_SECOND:
            return style.y;
        default:
            return style.x;
    }
}

#ifndef WR_DX11
void main(void) {
#else
void main(in a2v IN, out v2p OUT) {
    vec3 aPosition = IN.pos;
    ivec4 aDataA = IN.data0;
    ivec4 aDataB = IN.data1;
    int gl_VertexID = IN.vertexId;
#endif
    Primitive prim = load_primitive(aDataA, aDataB);
    Border border = fetch_border(prim.specific_prim_address);
    int sub_part = prim.user_data0;
    BorderCorners corners = get_border_corners(border, prim.local_rect);

    vec2 p0, p1;

    // TODO(gw): We'll need to pass through multiple styles
    //           once we support style transitions per corner.
    int style;
    vec4 edge_distances;
    vec4 color0, color1;
    vec2 color_delta;

    // TODO(gw): Now that all border styles are supported, the switch
    //           statement below can be tidied up quite a bit.

    switch (sub_part) {
        case 0: {
            p0 = corners.tl_outer;
            p1 = corners.tl_inner;
            color0 = border.colors[0];
            color1 = border.colors[1];
            SHADER_OUT(vClipCenter, corners.tl_outer + border.radii[0].xy);
            SHADER_OUT(vClipSign, vec2(1.0, 1.0));
            style = select_style(prim.user_data1, border.style.yx);
            vec4 adjusted_widths = get_effective_border_widths(border, style);
            vec4 inv_adjusted_widths = border.widths - adjusted_widths;
            set_radii(style,
                      border.radii[0].xy,
                      border.widths.xy,
                      adjusted_widths.xy
#ifdef WR_DX11
                      , OUT.vRadii0
                      , OUT.vRadii1
#endif
                      );
            set_edge_line(border.widths.xy,
                          corners.tl_outer,
                          vec2(1.0, 1.0)
#ifdef WR_DX11
                          , OUT.vColorEdgeLine
#endif
                          );
            edge_distances = vec4(p0 + adjusted_widths.xy,
                                  p0 + inv_adjusted_widths.xy);
            color_delta = vec2(1.0, 1.0);
            break;
        }
        case 1: {
            p0 = vec2(corners.tr_inner.x, corners.tr_outer.y);
            p1 = vec2(corners.tr_outer.x, corners.tr_inner.y);
            color0 = border.colors[1];
            color1 = border.colors[2];
            SHADER_OUT(vClipCenter, corners.tr_outer + vec2(-border.radii[0].z, border.radii[0].w));
            SHADER_OUT(vClipSign, vec2(-1.0, 1.0));
            style = select_style(prim.user_data1, border.style.zy);
            vec4 adjusted_widths = get_effective_border_widths(border, style);
            vec4 inv_adjusted_widths = border.widths - adjusted_widths;
            set_radii(style,
                      border.radii[0].zw,
                      border.widths.zy,
                      adjusted_widths.zy
#ifdef WR_DX11
                      , OUT.vRadii0
                      , OUT.vRadii1
#endif
                      );
            set_edge_line(border.widths.zy,
                          corners.tr_outer,
                          vec2(-1.0, 1.0)
#ifdef WR_DX11
                          , OUT.vColorEdgeLine
#endif
                          );
            edge_distances = vec4(p1.x - adjusted_widths.z,
                                  p0.y + adjusted_widths.y,
                                  p1.x - border.widths.z + adjusted_widths.z,
                                  p0.y + inv_adjusted_widths.y);
            color_delta = vec2(1.0, -1.0);
            break;
        }
        case 2: {
            p0 = corners.br_inner;
            p1 = corners.br_outer;
            color0 = border.colors[2];
            color1 = border.colors[3];
            SHADER_OUT(vClipCenter, corners.br_outer - border.radii[1].xy);
            SHADER_OUT(vClipSign, vec2(-1.0, -1.0));
            style = select_style(prim.user_data1, border.style.wz);
            vec4 adjusted_widths = get_effective_border_widths(border, style);
            vec4 inv_adjusted_widths = border.widths - adjusted_widths;
            set_radii(style,
                      border.radii[1].xy,
                      border.widths.zw,
                      adjusted_widths.zw
#ifdef WR_DX11
                      , OUT.vRadii0
                      , OUT.vRadii1
#endif
                      );
            set_edge_line(border.widths.zw,
                          corners.br_outer,
                          vec2(-1.0, -1.0)
#ifdef WR_DX11
                          , OUT.vColorEdgeLine
#endif

                          );
            edge_distances = vec4(p1.x - adjusted_widths.z,
                                  p1.y - adjusted_widths.w,
                                  p1.x - border.widths.z + adjusted_widths.z,
                                  p1.y - border.widths.w + adjusted_widths.w);
            color_delta = vec2(-1.0, -1.0);
            break;
        }
        case 3: {
            p0 = vec2(corners.bl_outer.x, corners.bl_inner.y);
            p1 = vec2(corners.bl_inner.x, corners.bl_outer.y);
            color0 = border.colors[3];
            color1 = border.colors[0];
            SHADER_OUT(vClipCenter, corners.bl_outer + vec2(border.radii[1].z, -border.radii[1].w));
            SHADER_OUT(vClipSign, vec2(1.0, -1.0));
            style = select_style(prim.user_data1, border.style.xw);
            vec4 adjusted_widths = get_effective_border_widths(border, style);
            vec4 inv_adjusted_widths = border.widths - adjusted_widths;
            set_radii(style,
                      border.radii[1].zw,
                      border.widths.xw,
                      adjusted_widths.xw
#ifdef WR_DX11
                      , OUT.vRadii0
                      , OUT.vRadii1
#endif
                      );
            set_edge_line(border.widths.xw,
                          corners.bl_outer,
                          vec2(1.0, -1.0)
#ifdef WR_DX11
                          , OUT.vColorEdgeLine
#endif
                          );
            edge_distances = vec4(p0.x + adjusted_widths.x,
                                  p1.y - adjusted_widths.w,
                                  p0.x + inv_adjusted_widths.x,
                                  p1.y - border.widths.w + adjusted_widths.w);
            color_delta = vec2(-1.0, 1.0);
            break;
        }
        default:
            p0 = corners.tl_outer;
            p1 = corners.tl_inner;
            color0 = border.colors[0];
            color1 = border.colors[1];
            SHADER_OUT(vClipCenter, corners.tl_outer + border.radii[0].xy);
            SHADER_OUT(vClipSign, vec2(1.0, 1.0));
            style = select_style(prim.user_data1, border.style.yx);
            vec4 adjusted_widths = get_effective_border_widths(border, style);
            vec4 inv_adjusted_widths = border.widths - adjusted_widths;
            set_radii(style,
                      border.radii[0].xy,
                      border.widths.xy,
                      adjusted_widths.xy
#ifdef WR_DX11
                      , OUT.vRadii0
                      , OUT.vRadii1
#endif
                      );
            set_edge_line(border.widths.xy,
                          corners.tl_outer,
                          vec2(1.0, 1.0)
#ifdef WR_DX11
                          , OUT.vColorEdgeLine
#endif
                          );
            edge_distances = vec4(p0 + adjusted_widths.xy,
                                  p0 + inv_adjusted_widths.xy);
            color_delta = vec2(1.0, 1.0);
            break;
    }

    switch (style) {
        case BORDER_STYLE_DOUBLE: {
            SHADER_OUT(vEdgeDistance, edge_distances);
            SHADER_OUT(vAlphaSelect, 0.0);
            SHADER_OUT(vSDFSelect, 0.0);
            break;
        }
        case BORDER_STYLE_GROOVE:
        case BORDER_STYLE_RIDGE:
            SHADER_OUT(vEdgeDistance, vec4(edge_distances.xy, 0.0, 0.0));
            SHADER_OUT(vAlphaSelect, 1.0);
            SHADER_OUT(vSDFSelect, 1.0);
            break;
        case BORDER_STYLE_DOTTED:
            // Disable normal clip radii for dotted corners, since
            // all the clipping is handled by the clip mask.
            SHADER_OUT(vClipSign, vec2(0.0, 0.0));
            SHADER_OUT(vEdgeDistance, vec4(0.0, 0.0, 0.0, 0.0));
            SHADER_OUT(vAlphaSelect, 1.0);
            SHADER_OUT(vSDFSelect, 0.0);
            break;
        default: {
            SHADER_OUT(vEdgeDistance, vec4(0.0, 0.0, 0.0, 0.0));
            SHADER_OUT(vAlphaSelect, 1.0);
            SHADER_OUT(vSDFSelect, 0.0);
            break;
        }
    }

    write_color(color0,
                color1,
                style,
                color_delta,
                prim.user_data1
#ifdef WR_DX11
                , OUT.vColor00
                , OUT.vColor01
                , OUT.vColor10
                , OUT.vColor11
#endif
                );

    RectWithSize segment_rect;
    segment_rect.p0 = p0;
    segment_rect.size = p1 - p0;

#ifdef WR_FEATURE_TRANSFORM
    TransformVertexInfo vi = write_transform_vertex(gl_VertexID,
                                                    segment_rect,
                                                    prim.local_clip_rect,
                                                    prim.z,
                                                    prim.layer,
                                                    prim.task,
                                                    prim.local_rect
#ifdef WR_DX11
                                                    , OUT.Position
                                                    , OUT.vLocalBounds
#endif
                                                    );
#else
    VertexInfo vi = write_vertex(aPosition,
                                 segment_rect,
                                 prim.local_clip_rect,
                                 prim.z,
                                 prim.layer,
                                 prim.task,
                                 prim.local_rect
#ifdef WR_DX11
                                 , OUT.Position
#endif
                                 );
#endif

    SHADER_OUT(vLocalPos, vi.local_pos);
    write_clip(vi.screen_pos,
               prim.clip_area
#ifdef WR_DX11
               , OUT.vClipMaskUvBounds
               , OUT.vClipMaskUv
#endif
               );
}
