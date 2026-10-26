/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `isnan()` for drivers that compile GLSL with fast math.
//!
//! Apple's OpenGL driver compiles shaders with fast math, which lets the compiler assume that no
//! value is NaN. `isnan(x)` and `x != x` are folded to `false`, and so is the emulation that
//! ANGLE adds for `isnan()` (`(x > 0.0 || x < 0.0) ? false : x != 0.0`), because it is made of
//! comparisons too. Shaders that mark a value as "unset" with NaN then read it as a number:
//! cosmos.gl draws every point at its largest size and with a garbage color.
//!
//! A NaN is a number with all exponent bits set and a non-zero mantissa, which `floatBitsToUint`
//! can test without any comparison on floats. ANGLE's `isnan_emu` functions are replaced by that
//! test in the translated GLSL. Nothing upstream models this: ANGLE's own workaround is the
//! comparison form, and its Metal backend, the one that Chrome and Safari use on this hardware,
//! turns fast math off.

/// The overloads that ANGLE's `isnan` emulation defines: the argument type, the result type and
/// the unsigned integer type of the same size.
const OVERLOADS: [(&str, &str, &str); 4] = [
    ("float", "bool", "uint"),
    ("vec2", "bvec2", "uvec2"),
    ("vec3", "bvec3", "uvec3"),
    ("vec4", "bvec4", "uvec4"),
];

/// Whether `floatBitsToUint` is part of the language of this translated shader, which is GLSL 3.30
/// or later and not ESSL.
fn has_float_bits(source: &str) -> bool {
    let Some(rest) = source.trim_start().strip_prefix("#version") else {
        return false;
    };
    let line = rest.lines().next().unwrap_or("");
    let mut words = line.split_whitespace();
    let version = words.next().and_then(|word| word.parse::<u32>().ok());
    version.is_some_and(|version| version >= 330) && words.all(|word| word != "es")
}

/// The index of the brace that closes the one at `open`.
fn closing_brace(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in source.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            },
            _ => {},
        }
    }
    None
}

/// Replaces the bodies of the `isnan_emu` functions in GLSL that ANGLE translated, so that they
/// do not depend on comparisons of NaN. Other shaders are returned as they are.
pub(crate) fn isnan_by_bits(mut source: String) -> String {
    if !has_float_bits(&source) {
        return source;
    }
    for (ty, bool_ty, uint_ty) in OVERLOADS {
        let header = format!("{bool_ty} isnan_emu({ty} x)");
        let Some(start) = source.find(&header) else {
            continue;
        };
        let Some(open) = source[start..].find('{').map(|offset| start + offset) else {
            continue;
        };
        let Some(close) = closing_brace(&source, open) else {
            continue;
        };
        let body = if ty == "float" {
            "return (floatBitsToUint(x) & 0x7fffffffu) > 0x7f800000u;".to_owned()
        } else {
            format!(
                "return greaterThan(floatBitsToUint(x) & {uint_ty}(0x7fffffffu), \
                 {uint_ty}(0x7f800000u));"
            )
        };
        source.replace_range(open + 1..close, &format!(" {body} "));
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    // The function emulation that ANGLE 0.7 writes at the top of a GLSL 4.10 vertex shader.
    const FLOAT: &str = "#version 410\n\
        // BEGIN: Generated code for built-in function emulation\n\
        \n\
        #define emu_precision\n\
        \n\
        bool isnan_emu(float x) { return (x > 0.0 || x < 0.0) ? false : x != 0.0; }\n\
        \n\
        // END: Generated code for built-in function emulation\n\
        \n\
        void main(){ if (isnan_emu(1.0)) { gl_Position = vec4(0.0); } }\n";

    const VEC4: &str = "#version 410\n\
        bvec4 isnan_emu(vec4 x)\n\
        {\n\
        \x20   bvec4 isnan;\n\
        \x20   for (int i = 0; i < 4; i++)\n\
        \x20   {\n\
        \x20       isnan[i] = (x[i] > 0.0 || x[i] < 0.0) ? false : x[i] != 0.0;\n\
        \x20   }\n\
        \x20   return isnan;\n\
        }\n\
        void main(){ gl_Position = vec4(isnan_emu(vec4(1.0)).x); }\n";

    #[test]
    fn the_float_function_tests_the_bits() {
        let fixed = isnan_by_bits(FLOAT.to_owned());
        assert!(fixed.contains(
            "bool isnan_emu(float x) { return (floatBitsToUint(x) & 0x7fffffffu) > 0x7f800000u; }"
        ));
        assert!(!fixed.contains("x != 0.0"));
        // Everything else is as ANGLE wrote it.
        assert!(fixed.contains("void main(){ if (isnan_emu(1.0)) { gl_Position = vec4(0.0); } }"));
        assert!(fixed.starts_with("#version 410\n"));
    }

    #[test]
    fn the_vector_functions_test_the_bits() {
        let fixed = isnan_by_bits(VEC4.to_owned());
        assert!(fixed.contains(
            "bvec4 isnan_emu(vec4 x)\n{ return greaterThan(floatBitsToUint(x) & \
             uvec4(0x7fffffffu), uvec4(0x7f800000u)); }\nvoid main()"
        ));
        assert!(!fixed.contains("for (int i"));
    }

    #[test]
    fn shaders_before_glsl_330_are_left_alone() {
        let old = FLOAT.replace("#version 410", "#version 150");
        assert_eq!(isnan_by_bits(old.clone()), old);
        let essl = FLOAT.replace("#version 410", "#version 300 es");
        assert_eq!(isnan_by_bits(essl.clone()), essl);
    }

    #[test]
    fn shaders_without_the_emulation_are_left_alone() {
        let plain = "#version 410\nvoid main(){ gl_Position = vec4(0.0); }\n".to_owned();
        assert_eq!(isnan_by_bits(plain.clone()), plain);
    }
}
