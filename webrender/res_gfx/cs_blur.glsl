/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include shared,prim_shared

varying vec3 vUv;
flat varying vec4 vUvRect;
flat varying vec2 vOffsetScale;
flat varying float vSigma;
// The number of pixels on each end that we apply the blur filter over.
flat varying int vSupport;

#ifdef WR_VERTEX_SHADER
// Applies a separable gaussian blur in one direction, as specified
// by the dir field in the blur command.

#define DIR_HORIZONTAL  0
#define DIR_VERTICAL    1

in int aBlurRenderTaskAddress;
in int aBlurSourceTaskAddress;
in int aBlurDirection;

struct BlurTask {
    RenderTaskCommonData common_data;
    float blur_radius;
};

BlurTask fetch_blur_task(int address) {
    RenderTaskData task_data = fetch_render_task_data(address);

    BlurTask task = BlurTask(
        task_data.common_data,
        task_data.user_data.x
    );

    return task;
}

void main(void) {
    BlurTask blur_task = fetch_blur_task(aBlurRenderTaskAddress);
    RenderTaskCommonData src_task = fetch_render_task_common_data(aBlurSourceTaskAddress);

    RectWithSize src_rect = src_task.task_rect;
    RectWithSize target_rect = blur_task.common_data.task_rect;

    vec2 texture_size = vec2(0.0);

    if (color_target) {
        texture_size = vec2(textureSize(sPrevPassColor, 0).xy);
    } else {
        texture_size = vec2(textureSize(sPrevPassAlpha, 0).xy);
    }
    vUv.z = src_task.texture_layer_index;
    vSigma = blur_task.blur_radius;

    // Ensure that the support is an even number of pixels to simplify the
    // fragment shader logic.
    //
    // TODO(pcwalton): Actually make use of this fact and use the texture
    // hardware for linear filtering.
    vSupport = int(ceil(1.5 * blur_task.blur_radius)) * 2;

    switch (aBlurDirection) {
        case DIR_HORIZONTAL:
            vOffsetScale = vec2(1.0 / texture_size.x, 0.0);
            break;
        case DIR_VERTICAL:
            vOffsetScale = vec2(0.0, 1.0 / texture_size.y);
            break;
        default:
            vOffsetScale = vec2(0.0);
    }

    vUvRect = vec4(src_rect.p0 + vec2(0.5),
                   src_rect.p0 + src_rect.size - vec2(0.5));
    vUvRect /= texture_size.xyxy;

    vec2 pos = target_rect.p0 + target_rect.size * aPosition.xy;

    vec2 uv0 = src_rect.p0 / texture_size;
    vec2 uv1 = (src_rect.p0 + src_rect.size) / texture_size;
    vUv.xy = mix(uv0, uv1, aPosition.xy);

    gl_Position = uTransform * vec4(pos, 0.0, 1.0);
}
#endif

#ifdef WR_FRAGMENT_SHADER

// TODO(gw): Write a fast path blur that handles smaller blur radii
//           with a offset / weight uniform table and a constant
//           loop iteration count!

// TODO(gw): Make use of the bilinear sampling trick to reduce
//           the number of texture fetches needed for a gaussian blur.

void main(void) {
    if (color_target) {
        vec4 original_color = texture(sPrevPassColor, vUv);
         // TODO(gw): The gauss function gets NaNs when blur radius
        //           is zero. In the future, detect this earlier
        //           and skip the blur passes completely.
        if (vSupport == 0) {
            oFragColor = vec4(original_color);
            return;
        }
         // Incremental Gaussian Coefficent Calculation (See GPU Gems 3 pp. 877 - 889)
        vec3 gauss_coefficient;
        gauss_coefficient.x = 1.0 / (sqrt(2.0 * 3.14159265) * vSigma);
        gauss_coefficient.y = exp(-0.5 / (vSigma * vSigma));
        gauss_coefficient.z = gauss_coefficient.y * gauss_coefficient.y;
         float gauss_coefficient_sum = 0.0;
        vec4 avg_color = original_color * gauss_coefficient.x;
        gauss_coefficient_sum += gauss_coefficient.x;
        gauss_coefficient.xy *= gauss_coefficient.yz;
         for (int i=1 ; i <= vSupport ; ++i) {
            vec2 offset = vOffsetScale * float(i);
             vec2 st0 = clamp(vUv.xy - offset, vUvRect.xy, vUvRect.zw);
            avg_color += texture(sPrevPassColor, vec3(st0, vUv.z)) * gauss_coefficient.x;
             vec2 st1 = clamp(vUv.xy + offset, vUvRect.xy, vUvRect.zw);
            avg_color += texture(sPrevPassColor, vec3(st1, vUv.z)) * gauss_coefficient.x;
             gauss_coefficient_sum += 2.0 * gauss_coefficient.x;
            gauss_coefficient.xy *= gauss_coefficient.yz;
        }
        oFragColor = vec4(avg_color) / gauss_coefficient_sum;
    } else {
        float original_color = texture(sPrevPassAlpha, vUv).r;
         // TODO(gw): The gauss function gets NaNs when blur radius
        //           is zero. In the future, detect this earlier
        //           and skip the blur passes completely.
        if (vSupport == 0) {
            oFragColor = vec4(original_color);
            return;
        }
         // Incremental Gaussian Coefficent Calculation (See GPU Gems 3 pp. 877 - 889)
        vec3 gauss_coefficient;
        gauss_coefficient.x = 1.0 / (sqrt(2.0 * 3.14159265) * vSigma);
        gauss_coefficient.y = exp(-0.5 / (vSigma * vSigma));
        gauss_coefficient.z = gauss_coefficient.y * gauss_coefficient.y;
         float gauss_coefficient_sum = 0.0;
        float avg_color = original_color * gauss_coefficient.x;
        gauss_coefficient_sum += gauss_coefficient.x;
        gauss_coefficient.xy *= gauss_coefficient.yz;
         for (int i=1 ; i <= vSupport ; ++i) {
            vec2 offset = vOffsetScale * float(i);
             vec2 st0 = clamp(vUv.xy - offset, vUvRect.xy, vUvRect.zw);
            avg_color += texture(sPrevPassAlpha, vec3(st0, vUv.z)).r * gauss_coefficient.x;
             vec2 st1 = clamp(vUv.xy + offset, vUvRect.xy, vUvRect.zw);
            avg_color += texture(sPrevPassAlpha, vec3(st1, vUv.z)).r * gauss_coefficient.x;
             gauss_coefficient_sum += 2.0 * gauss_coefficient.x;
            gauss_coefficient.xy *= gauss_coefficient.yz;
        }
        oFragColor = vec4(avg_color) / gauss_coefficient_sum;
    }
}
#endif