use crate::{
    engine::{general_purpose::INVALID_VALUE, DecodeMetadata, DecodePaddingMode},
    DecodeError, PAD_BYTE,
};

/// Decode the last 0-4 bytes, checking for trailing set bits and padding per the provided
/// parameters.
///
/// Returns the decode metadata representing the total number of bytes decoded, including the ones
/// indicated as already written by `output_index`.
pub(crate) fn decode_suffix(
    input: &[u8],
    input_index: usize,
    output: &mut [u8],
    mut output_index: usize,
    decode_table: &[u8; 256],
    decode_allow_trailing_bits: bool,
    padding_mode: DecodePaddingMode,
) -> Result<DecodeMetadata, DecodeError> {
    debug_assert!((input.len() - input_index) <= 4);

    // Decode any leftovers that might not be a complete input chunk of 8 bytes.
    // Use a u64 as a stack-resident 8 byte buffer.
    let mut morsels_in_leftover = 0;
    let mut padding_bytes_count = 0;
    // offset from input_index
    let mut first_padding_offset: usize = 0;
    let mut last_symbol = 0_u8;
    let mut morsels = [0_u8; 4];

    for (leftover_index, &b) in input[input_index..].iter().enumerate() {
        // '=' padding
        if b == PAD_BYTE {
            // There can be bad padding bytes in a few ways:
            // 1 - Padding with non-padding characters after it
            // 2 - Padding after zero or one characters in the current quad (should only
            //     be after 2 or 3 chars)
            // 3 - More than two characters of padding. If 3 or 4 padding chars
            //     are in the same quad, that implies it will be caught by #2.
            //     If it spreads from one quad to another, it will be an invalid byte
            //     in the first quad.
            // 4 - Non-canonical padding -- 1 byte when it should be 2, etc.
            //     Per config, non-canonical but still functional non- or partially-padded base64
            //     may be treated as an error condition.

            if leftover_index < 2 {
                // Check for case #2.
                let bad_padding_index = input_index
                    + if padding_bytes_count > 0 {
                        // If we've already seen padding, report the first padding index.
                        // This is to be consistent with the normal decode logic: it will report an
                        // error on the first padding character (since it doesn't expect to see
                        // anything but actual encoded data).
                        // This could only happen if the padding started in the previous quad since
                        // otherwise this case would have been hit at i == 4 if it was the same
                        // quad.
                        first_padding_offset
                    } else {
                        // haven't seen padding before, just use where we are now
                        leftover_index
                    };
                return Err(DecodeError::InvalidByte(bad_padding_index, b));
            }

            if padding_bytes_count == 0 {
                first_padding_offset = leftover_index;
            }

            padding_bytes_count += 1;
            continue;
        }

        // Check for case #1.
        // To make '=' handling consistent with the main loop, don't allow
        // non-suffix '=' in trailing chunk either. Report error as first
        // erroneous padding.
        if padding_bytes_count > 0 {
            return Err(DecodeError::InvalidByte(
                input_index + first_padding_offset,
                PAD_BYTE,
            ));
        }

        last_symbol = b;

        // can use up to 8 * 6 = 48 bits of the u64, if last chunk has no padding.
        // Pack the leftovers from left to right.
        let morsel = decode_table[b as usize];
        if morsel == INVALID_VALUE {
            return Err(DecodeError::InvalidByte(input_index + leftover_index, b));
        }

        morsels[morsels_in_leftover] = morsel;
        morsels_in_leftover += 1;
    }

    // If there was 1 trailing byte, and it was valid, and we got to this point without hitting
    // an invalid byte, now we can report invalid length
    if !input.is_empty() && morsels_in_leftover < 2 {
        return Err(DecodeError::InvalidLength(
            input_index + morsels_in_leftover,
        ));
    }

    match padding_mode {
        DecodePaddingMode::Indifferent => { /* everything we care about was already checked */ }
        DecodePaddingMode::RequireCanonical => {
            // allow empty input
            if (padding_bytes_count + morsels_in_leftover) % 4 != 0 {
                return Err(DecodeError::InvalidPadding);
            }
        }
        DecodePaddingMode::RequireNone => {
            if padding_bytes_count > 0 {
                // check at the end to make sure we let the cases of padding that should be InvalidByte
                // get hit
                return Err(DecodeError::InvalidPadding);
            }
        }
    }

    // When encoding 1 trailing byte (e.g. 0xFF), 2 base64 bytes ("/w") are needed.
    // / is the symbol for 63 (0x3F, bottom 6 bits all set) and w is 48 (0x30, top 2 bits
    // of bottom 6 bits set).
    // When decoding two symbols back to one trailing byte, any final symbol higher than
    // w would still decode to the original byte because we only care about the top two
    // bits in the bottom 6, but would be a non-canonical encoding. So, we calculate a
    // mask based on how many bits are used for just the canonical encoding, and optionally
    // error if any other bits are set. In the example of one encoded byte -> 2 symbols,
    // 2 symbols can technically encode 12 bits, but the last 4 are non canonical, and
    // useless since there are no more symbols to provide the necessary 4 additional bits
    // to finish the second original byte.

    let leftover_bytes_to_append = morsels_in_leftover * 6 / 8;
    // Put the up to 6 complete bytes as the high bytes.
    // Gain a couple percent speedup from nudging these ORs to use more ILP with a two-way split.
    let mut leftover_num = (u32::from(morsels[0]) << 26)
        | (u32::from(morsels[1]) << 20)
        | (u32::from(morsels[2]) << 14)
        | (u32::from(morsels[3]) << 8);

    // if there are bits set outside the bits we care about, last symbol encodes trailing bits that
    // will not be included in the output
    let mask = !0_u32 >> (leftover_bytes_to_append * 8);
    if !decode_allow_trailing_bits && (leftover_num & mask) != 0 {
        // last morsel is at `morsels_in_leftover` - 1
        return Err(DecodeError::InvalidLastSymbol(
            input_index + morsels_in_leftover - 1,
            last_symbol,
        ));
    }

    // Strangely, this approach benchmarks better than writing bytes one at a time,
    // or copy_from_slice into output.
    for _ in 0..leftover_bytes_to_append {
        let hi_byte = (leftover_num >> 24) as u8;
        leftover_num <<= 8;
        // TODO use checked writes
        output[output_index] = hi_byte;
        output_index += 1;
    }

    Ok(DecodeMetadata::new(
        output_index,
        if padding_bytes_count > 0 {
            Some(input_index + first_padding_offset)
        } else {
            None
        },
    ))
}
