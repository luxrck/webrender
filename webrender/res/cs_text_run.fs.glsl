//#line 1
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

void main(void) {
    float a = texture(sColor0, vUv).a;
    Target0 = vec4(vColor.rgb, vColor.a * a);
}
