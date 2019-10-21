/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

layout(constant_id = 0) const bool alpha_pass = false;
layout(constant_id = 1) const bool color_target = false;
layout(constant_id = 2) const bool glyph_transform_f = false;
layout(constant_id = 3) const bool dithering = false;
layout(constant_id = 4) const bool debug_overdraw = false;
layout(constant_id = 5) const bool repetition = false;
layout(constant_id = 6) const bool antialiasing = false;
layout(constant_id = 7) const bool push_constants = true;

#if defined(GL_ES)
    #if GL_ES == 1
        #ifdef GL_FRAGMENT_PRECISION_HIGH
        precision highp sampler2DArray;
        #else
        precision mediump sampler2DArray;
        #endif

        // Sampler default precision is lowp on mobile GPUs.
        // This causes RGBA32F texture data to be clamped to 16 bit floats on some GPUs (e.g. Mali-T880).
        // Define highp precision macro to allow lossless FLOAT texture sampling.
        #define HIGHP_SAMPLER_FLOAT highp

        // Default int precision in GLES 3 is highp (32 bits) in vertex shaders
        // and mediump (16 bits) in fragment shaders. If an int is being used as
        // a texel address in a fragment shader it, and therefore requires > 16
        // bits, it must be qualified with this.
        #define HIGHP_FS_ADDRESS highp

        // texelFetchOffset is buggy on some Android GPUs (see issue #1694).
        // Fallback to texelFetch on mobile GPUs.
        #define TEXEL_FETCH(sampler, position, lod, offset) texelFetch(sampler, position + offset, lod)
    #else
        #define HIGHP_SAMPLER_FLOAT
        #define HIGHP_FS_ADDRESS
        #define TEXEL_FETCH(sampler, position, lod, offset) texelFetchOffset(sampler, position, lod, offset)
    #endif
#else
    #define HIGHP_SAMPLER_FLOAT
    #define HIGHP_FS_ADDRESS
    #define TEXEL_FETCH(sampler, position, lod, offset) texelFetchOffset(sampler, position, lod, offset)
#endif

#ifdef WR_VERTEX_SHADER
    #define varying out
#endif

#ifdef WR_FRAGMENT_SHADER
    precision highp float;
    #define varying in
#endif
