/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

layout(location = 6) flat varying vec4 vColor;
layout(location = 7) varying vec2 vUv;
layout(location = 8) flat varying vec4 vUvBorder;

#ifdef WR_FEATURE_TRANSFORM
layout(location = 9) varying vec3 vLocalPos;
#endif
