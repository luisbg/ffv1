use crate::constants::CONTEXT_SIZE;
use crate::crc32mpeg2::crc32_mpeg2;
use crate::error::{Error, Result};
use crate::golomb::{Coder, State};
use crate::jpeg2000rct::{rct16, rct8, rct_mid};
use crate::pred::{derive_borders, get_context, get_median};
use crate::range::RangeCoder;
use crate::rangecoder::tables::DEFAULT_STATE_TRANSITION;
use crate::record::ConfigRecord;
use crate::slice::{count_slices, is_keyframe, InternalFrame, Slice};

/// Frame contains a decoded FFV1 frame and relevant
/// data about the frame.
///
/// If BitDepth is 8, image data is in Buf. If it is anything else,
/// image data is in Buf16.
///
/// Image data consists of up to four contiguous planes, as follows:
///   - If ColorSpace is YCbCr:
///     - Plane 0 is Luma (always present)
///     - If HasChroma is true, the next two planes are Cr and Cr, subsampled by
///       ChromaSubsampleV and ChromaSubsampleH.
///     - If HasAlpha is true, the next plane is alpha.
///  - If ColorSpace is RGB:
///    - Plane 0 is Green
///    - Plane 1 is Blue
///    - Plane 2 is Red
///    - If HasAlpha is true, plane 4 is alpha.
pub struct Frame {
    /// Image data. Valid only when BitDepth is 8.
    pub buf: Vec<Vec<u8>>,
    /// Image data. Valid only when BitDepth is greater than 8.
    pub buf16: Vec<Vec<u16>>,
    /// Unexported 32-bit scratch buffer for 16-bit JPEG2000-RCT RGB
    pub buf32: Vec<Vec<u32>>,
    /// Width of the frame, in pixels.
    #[allow(dead_code)]
    pub width: u32,
    /// Height of the frame, in pixels.
    #[allow(dead_code)]
    pub height: u32,
    /// Bitdepth of the frame (8-16).
    #[allow(dead_code)]
    pub bit_depth: u8,
    /// Colorspace of the frame. See the colorspace constants.
    #[allow(dead_code)]
    pub color_space: isize,
    /// Whether or not chroma planes are present.
    #[allow(dead_code)]
    pub has_chroma: bool,
    /// Whether or not an alpha plane is present.
    #[allow(dead_code)]
    pub has_alpha: bool,
    /// The log2 vertical chroma subampling value.
    #[allow(dead_code)]
    pub chroma_subsample_v: u8,
    /// The log2 horizontal chroma subsampling value.
    #[allow(dead_code)]
    pub chroma_subsample_h: u8,
}

/// Decoder is a FFV1 decoder instance.
pub struct Decoder {
    width: u32,
    height: u32,
    record: ConfigRecord,
    state_transition: [u8; 256],
    initial_states: Vec<Vec<Vec<u8>>>, // FIXME: This is horrible
    current_frame: InternalFrame,
}

impl Decoder {
    /// NewDecoder creates a new FFV1 decoder instance.
    ///
    /// 'record' is the codec private data provided by the container. For
    /// Matroska, this is what is in CodecPrivate (adjusted for e.g. VFW
    /// data that may be before it). For ISOBMFF, this is the 'glbl' box.
    ///
    /// 'width' and 'height' are the frame width and height provided by
    /// the container.
    pub fn new(record: &[u8], width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidInputData(format!(
                "invalid dimensions: {}x{}",
                width, height
            )));
        }

        if record.is_empty() {
            return Err(Error::InvalidInputData(
                "invalid record with length zero".to_owned(),
            ));
        }

        let record = match ConfigRecord::parse_config_record(&record) {
            Ok(record) => record,
            Err(err) => {
                return Err(Error::InvalidInputData(format!(
                    "invalid v3 configuration record: {}",
                    err
                )))
            }
        };

        let mut decoder = Decoder {
            width,
            height,
            record,
            state_transition: [0; 256],
            initial_states: Vec::new(),
            current_frame: InternalFrame {
                keyframe: false,
                slice_info: Vec::new(),
                slices: Vec::new(),
            },
        };

        decoder.initialize_states();

        Ok(decoder)
    }

    /// DecodeFrame takes a packet and decodes it to a ffv1.Frame.
    ///
    /// Slice threading is used by default, with one goroutine per
    /// slice.
    pub fn decode_frame(&mut self, frame_input: &[u8]) -> Result<Frame> {
        let mut frame = Frame {
            buf: Vec::new(),
            buf16: Vec::new(),
            buf32: Vec::new(),
            width: self.width,
            height: self.height,
            bit_depth: self.record.bits_per_raw_sample,
            color_space: self.record.colorspace_type as isize,
            has_chroma: self.record.chroma_planes,
            has_alpha: self.record.extra_plane,
            chroma_subsample_v: if self.record.chroma_planes {
                self.record.log2_v_chroma_subsample
            } else {
                0
            },
            chroma_subsample_h: if self.record.chroma_planes {
                self.record.log2_h_chroma_subsample
            } else {
                0
            },
        };

        let mut num_planes = 1;
        if self.record.chroma_planes {
            num_planes += 2;
        }
        if self.record.extra_plane {
            num_planes += 1;
        }

        // Hideous and temporary.
        if self.record.bits_per_raw_sample == 8 {
            frame.buf = vec![Vec::new(); num_planes];
            frame.buf[0] = vec![0; (self.width * self.height) as usize];
            if self.record.chroma_planes {
                let chroma_width =
                    self.width >> self.record.log2_h_chroma_subsample;
                let chroma_height =
                    self.height >> self.record.log2_v_chroma_subsample;
                frame.buf[1] =
                    vec![0; (chroma_width * chroma_height) as usize];
                frame.buf[2] =
                    vec![0; (chroma_width * chroma_height) as usize];
            }
            if self.record.extra_plane {
                frame.buf[3] = vec![0; (self.width * self.height) as usize];
            }
        }

        // We allocate *both* if it's 8bit RGB since I'm a terrible person and
        // I wanted to use it as a scratch space, since JPEG2000-RCT is very
        // annoyingly coded as n+1 bits, and I wanted the implementation
        // to be straightforward... RIP.
        if self.record.bits_per_raw_sample > 8
            || self.record.colorspace_type == 1
        {
            frame.buf16 = vec![Vec::new(); num_planes];
            frame.buf16[0] = vec![0; (self.width * self.height) as usize];
            if self.record.chroma_planes {
                let chroma_width =
                    self.width >> self.record.log2_h_chroma_subsample;
                let chroma_height =
                    self.height >> self.record.log2_v_chroma_subsample;
                frame.buf16[1] =
                    vec![0; (chroma_width * chroma_height) as usize];
                frame.buf16[2] =
                    vec![0; (chroma_width * chroma_height) as usize];
            }
            if self.record.extra_plane {
                frame.buf16[3] = vec![0; (self.width * self.height) as usize];
            }
        }

        // For 16-bit RGB we need a 32-bit scratch space beause we need to predict
        // based on 17-bit values in the JPEG2000-RCT space, so just allocate a
        // whole frame, because I am lazy. Is it slow? Yes.
        if self.record.bits_per_raw_sample == 16
            && self.record.colorspace_type == 1
        {
            frame.buf32 = vec![Vec::new(); num_planes];
            frame.buf32[0] = vec![0; (self.width * self.height) as usize];
            frame.buf32[1] = vec![0; (self.width * self.height) as usize];
            frame.buf32[2] = vec![0; (self.width * self.height) as usize];
            if self.record.extra_plane {
                frame.buf32[3] = vec![0; (self.width * self.height) as usize];
            }
        }

        // We parse the frame's keyframe info outside the slice decoding
        // loop so we know ahead of time if each slice has to refresh its
        // states or not. This allows easy slice threading.
        self.current_frame.keyframe = is_keyframe(frame_input);

        // We parse all the footers ahead of time too, for the same reason.
        // It allows us to know all the slice positions and sizes.
        //
        // See: 9.1.1. Multi-threading Support and Independence of Slices
        let err = self.parse_footers(frame_input);
        if let Err(err) = err {
            return Err(Error::FrameError(format!(
                "invalid frame footer: {}",
                err
            )));
        }

        // Slice threading lazymode (not using sync for now, only sequential code,
        // FIXME there could be errors here)
        for i in 0..self.current_frame.slices.len() {
            let err = self.decode_slice(frame_input, i as isize, &mut frame);
            if let Err(err) = err {
                return Err(Error::SliceError(format!(
                    "slice {} failed: {}",
                    i, err
                )));
            }
        }

        // Delete the scratch buffer, if needed, as per above.
        if self.record.bits_per_raw_sample == 8
            && self.record.colorspace_type == 1
        {
            frame.buf16 = Vec::new();
        }

        // We'll never need this again.
        frame.buf32 = Vec::new();

        Ok(frame)
    }

    /// Initializes initial state for the range coder.
    ///
    /// See: 4.1.15. initial_state_delta
    fn initialize_states(&mut self) {
        for (i, default_state_transition) in
            DEFAULT_STATE_TRANSITION.iter().enumerate().skip(1)
        {
            self.state_transition[i] = (*default_state_transition as i16
                + self.record.state_transition_delta[i])
                as u8;
        }

        self.initial_states =
            vec![Vec::new(); self.record.initial_state_delta.len()];
        for i in 0..self.record.initial_state_delta.len() {
            self.initial_states[i] =
                vec![Vec::new(); self.record.initial_state_delta[i].len()];
            for j in 0..self.record.initial_state_delta[i].len() {
                self.initial_states[i][j] =
                    vec![0; self.record.initial_state_delta[i][j].len()];
                for k in 0..self.record.initial_state_delta[i][j].len() {
                    let mut pred = 128 as i16;
                    if j != 0 {
                        pred = self.initial_states[i][j - 1][k] as i16;
                    }
                    self.initial_states[i][j][k] =
                        ((pred + self.record.initial_state_delta[i][j][k])
                            & 255) as u8;
                }
            }
        }
    }

    /// Parses all footers in a frame and allocates any necessary slice structures.
    ///
    /// See: * 9.1.1. Multi-threading Support and Independence of Slices
    ///      * 3.8.1.3. Initial Values for the Context Model
    ///      * 3.8.2.4. Initial Values for the VLC context state
    pub fn parse_footers(&mut self, buf: &[u8]) -> Result<()> {
        let err =
            count_slices(buf, &mut self.current_frame, self.record.ec != 0);
        if let Err(err) = err {
            return Err(Error::SliceError(format!(
                "couldn't count slices: {}",
                err
            )));
        }

        let mut slices: Vec<Slice> =
            vec![Default::default(); self.current_frame.slice_info.len()];
        if !self.current_frame.keyframe {
            if slices.len() != self.current_frame.slices.len() {
                return Err(Error::SliceError("inter frames must have the same number of slices as the preceding intra frame".to_owned()));
            }
            for (i, slice) in slices.iter_mut().enumerate() {
                slice.state = self.current_frame.slices[i].state.clone();
            }
            if self.record.coder_type == 0 {
                for (i, slice) in slices.iter_mut().enumerate() {
                    slice.golomb_state =
                        self.current_frame.slices[i].golomb_state.clone();
                }
            }
        }
        self.current_frame.slices = slices;

        Ok(())
    }

    /// Parses a slice's header.
    ///
    /// See: 4.5. Slice Header
    pub fn parse_slice_header(
        &mut self,
        coder: &mut RangeCoder,
        slicenum: usize,
    ) {
        // 4. Bitstream
        let mut slice_state: [u8; CONTEXT_SIZE as usize] =
            [128; CONTEXT_SIZE as usize];

        // 4.5.1. slice_x
        self.current_frame.slices[slicenum].header.slice_x =
            coder.ur(&mut slice_state);
        // 4.5.2. slice_y
        self.current_frame.slices[slicenum].header.slice_y =
            coder.ur(&mut slice_state);
        // 4.5.3 slice_width
        self.current_frame.slices[slicenum]
            .header
            .slice_width_minus1 = coder.ur(&mut slice_state);
        // 4.5.4 slice_height
        self.current_frame.slices[slicenum]
            .header
            .slice_height_minus1 = coder.ur(&mut slice_state);

        // 4.5.5. quant_table_set_index_count
        let mut quant_table_set_index_count = 1;
        if self.record.chroma_planes {
            quant_table_set_index_count += 1;
        }
        if self.record.extra_plane {
            quant_table_set_index_count += 1;
        }

        // 4.5.6. quant_table_set_index
        self.current_frame.slices[slicenum]
            .header
            .quant_table_set_index =
            vec![0; quant_table_set_index_count as usize];
        for i in 0..quant_table_set_index_count {
            self.current_frame.slices[slicenum]
                .header
                .quant_table_set_index[i] = coder.ur(&mut slice_state) as u8;
        }

        // 4.5.7. picture_structure
        self.current_frame.slices[slicenum].header.picture_structure =
            coder.ur(&mut slice_state) as u8;

        // It's really weird for slices within the same frame to code
        // their own SAR values...
        //
        // See: * 4.5.8. sar_num
        //      * 4.5.9. sar_den
        self.current_frame.slices[slicenum].header.sar_num =
            coder.ur(&mut slice_state);
        self.current_frame.slices[slicenum].header.sar_den =
            coder.ur(&mut slice_state);

        // Calculate bounaries for easy use elsewhere
        //
        // See: * 4.6.3. slice_pixel_height
        //      * 4.6.4. slice_pixel_y
        //      * 4.7.2. slice_pixel_width
        //      * 4.7.3. slice_pixel_x
        self.current_frame.slices[slicenum].start_x =
            self.current_frame.slices[slicenum].header.slice_x * self.width
                / (self.record.num_h_slices_minus1 as u32 + 1);
        self.current_frame.slices[slicenum].start_y =
            self.current_frame.slices[slicenum].header.slice_y * self.height
                / (self.record.num_v_slices_minus1 as u32 + 1);
        self.current_frame.slices[slicenum].width =
            ((self.current_frame.slices[slicenum].header.slice_x
                + self.current_frame.slices[slicenum]
                    .header
                    .slice_width_minus1
                + 1)
                * self.width
                / (self.record.num_h_slices_minus1 as u32 + 1))
                - self.current_frame.slices[slicenum].start_x;
        self.current_frame.slices[slicenum].height =
            ((self.current_frame.slices[slicenum].header.slice_y
                + self.current_frame.slices[slicenum]
                    .header
                    .slice_height_minus1
                + 1)
                * self.height
                / (self.record.num_v_slices_minus1 as u32 + 1))
                - self.current_frame.slices[slicenum].start_y;
    }

    /// Line decoding.
    ///
    /// So, so many arguments. I would have just inlined this whole thing
    /// but it needs to be separate because of RGB mode where every line
    /// is done in its entirety instead of per plane.
    ///
    /// Many could be refactored into being in the context, but I haven't
    /// got to it yet, so instead, I shall repent once for each function
    /// argument, twice daily.
    ///
    /// See: 4.7. Line
    #[allow(clippy::too_many_arguments)]
    pub fn decode_line(
        &mut self,
        coder: &mut RangeCoder,
        golomb_coder: &mut Option<&mut Coder>,
        slicenum: usize,
        frame: &mut Frame,
        width: isize,
        height: isize,
        stride: isize,
        offset: isize,
        yy: isize,
        plane: isize,
        qt: isize,
    ) {
        // Runs are horizontal and thus cannot run more than a line.
        //
        // See: 3.8.2.2.1. Run Length Coding
        if let Some(ref mut golomb_coder) = golomb_coder {
            golomb_coder.new_line();
        }

        // 4.7.4. sample_difference
        for x in 0..width as usize {
            // 3.8. Coding of the Sample Difference
            let mut shift = self.record.bits_per_raw_sample;
            if self.record.colorspace_type == 1 {
                shift = self.record.bits_per_raw_sample + 1;
            }

            // Derive neighbours
            //
            // See pred.go for details.
            #[allow(non_snake_case)]
            #[allow(clippy::many_single_char_names)]
            let (T, L, t, l, tr, tl) = if self.record.bits_per_raw_sample == 8
                && self.record.colorspace_type != 1
            {
                derive_borders(
                    &frame.buf[plane as usize][offset as usize..],
                    x as isize,
                    yy,
                    width,
                    height,
                    stride,
                )
            } else if self.record.bits_per_raw_sample == 16
                && self.record.colorspace_type == 1
            {
                derive_borders(
                    &frame.buf32[plane as usize][offset as usize..],
                    x as isize,
                    yy,
                    width,
                    height,
                    stride,
                )
            } else {
                derive_borders(
                    &frame.buf16[plane as usize][offset as usize..],
                    x as isize,
                    yy,
                    width,
                    height,
                    stride,
                )
            };

            // See pred.go for details.
            //
            // See also: * 3.4. Context
            //           * 3.6. Quantization Table Set Indexes
            let mut context = get_context(
                &self.record.quant_tables[self.current_frame.slices[slicenum]
                    .header
                    .quant_table_set_index[qt as usize]
                    as usize],
                T,
                L,
                t,
                l,
                tr,
                tl,
            );
            let sign = if context < 0 {
                context = -context;
                true
            } else {
                false
            };

            let mut diff = if let Some(ref mut golomb_coder) = golomb_coder {
                golomb_coder.sg(
                    context,
                    &mut self.current_frame.slices[slicenum].golomb_state
                        [qt as usize][context as usize],
                    shift as usize,
                )
            } else {
                coder.sr(&mut self.current_frame.slices[slicenum].state
                    [qt as usize][context as usize])
            };

            // 3.4. Context
            if sign {
                diff = -diff;
            }

            // 3.8. Coding of the Sample Difference
            let mut val = diff;
            if self.record.colorspace_type == 0
                && self.record.bits_per_raw_sample == 16
                && golomb_coder.is_none()
            {
                // 3.3. Median Predictor
                let left16s = if l >= 32768 { l - 65536 } else { l };
                let top16s = if t >= 32768 { t - 65536 } else { t };
                let diag16s = if tl >= 32768 { tl - 65536 } else { tl };

                val += get_median(left16s, top16s, left16s + top16s - diag16s)
                    as i32;
            } else {
                val += get_median(l, t, l + t - tl) as i32;
            }

            val &= (1 << shift) - 1;

            if self.record.bits_per_raw_sample == 8
                && self.record.colorspace_type != 1
            {
                frame.buf[plane as usize]
                    [offset as usize + (yy as usize * stride as usize) + x] =
                    val as u8;
            } else if self.record.bits_per_raw_sample == 16
                && self.record.colorspace_type == 1
            {
                frame.buf32[plane as usize]
                    [offset as usize + (yy as usize * stride as usize) + x] =
                    val as u32;
            } else {
                frame.buf16[plane as usize]
                    [offset as usize + (yy as usize * stride as usize) + x] =
                    val as u16;
            }
        }
    }

    /// Decoding happens here.
    ///
    /// See: * 4.6. Slice Content
    pub fn decode_slice_content(
        &mut self,
        coder: &mut RangeCoder,
        golomb_coder: &mut Option<&mut Coder>,
        slicenum: usize,
        frame: &mut Frame,
    ) {
        // 4.6.1. primary_color_count
        let mut primary_color_count = 1;
        let mut chroma_planes = 0;
        if self.record.chroma_planes {
            chroma_planes = 2;
            primary_color_count += 2;
        }
        if self.record.extra_plane {
            primary_color_count += 1;
        }

        if self.record.colorspace_type != 1 {
            // YCbCr Mode
            //
            // Planes are independent.
            //
            // See: 3.7.1. YCbCr
            for p in 0..primary_color_count {
                // See: * 4.6.2. plane_pixel_height
                //      * 4.7.1. plane_pixel_width
                let (
                    plane_pixel_height,
                    plane_pixel_width,
                    plane_pixel_stride,
                    start_x,
                    start_y,
                    quant_table,
                ) = if p == 0 || p == 1 + chroma_planes {
                    let quant_table = if p == 0 { 0 } else { chroma_planes };
                    (
                        self.current_frame.slices[slicenum].height as isize,
                        self.current_frame.slices[slicenum].width as isize,
                        self.width as isize,
                        self.current_frame.slices[slicenum].start_x as isize,
                        self.current_frame.slices[slicenum].start_y as isize,
                        quant_table,
                    )
                } else {
                    // This is, of course, silly, but I want to do it "by the spec".
                    (
                        (self.current_frame.slices[slicenum].height as f64
                            / (1 << self.record.log2_v_chroma_subsample)
                                as f64)
                            .ceil() as isize,
                        (self.current_frame.slices[slicenum].width as f64
                            / (1 << self.record.log2_h_chroma_subsample)
                                as f64)
                            .ceil() as isize,
                        (self.width as f64
                            / (1 << self.record.log2_h_chroma_subsample)
                                as f64)
                            .ceil() as isize,
                        (self.current_frame.slices[slicenum].start_x as f64
                            / ((1 << self.record.log2_v_chroma_subsample)
                                as f64))
                            .ceil() as isize,
                        (self.current_frame.slices[slicenum].start_y as f64
                            / ((1 << self.record.log2_h_chroma_subsample)
                                as f64))
                            .ceil() as isize,
                        1,
                    )
                };

                // 3.8.2.2.1. Run Length Coding
                if let Some(ref mut golomb_coder) = golomb_coder {
                    golomb_coder.new_plane(plane_pixel_width as u32);
                }

                for y in 0..plane_pixel_height {
                    let offset = start_y * plane_pixel_stride + start_x;
                    self.decode_line(
                        coder,
                        golomb_coder,
                        slicenum,
                        frame,
                        plane_pixel_width,
                        plane_pixel_height,
                        plane_pixel_stride,
                        offset,
                        y,
                        p,
                        quant_table,
                    );
                }
            }
        } else {
            // RGB (JPEG2000-RCT) Mode
            //
            // All planes are coded per line.
            //
            // See: 3.7.2. RGB
            if let Some(ref mut golomb_coder) = golomb_coder {
                golomb_coder.new_plane(
                    self.current_frame.slices[slicenum].width as u32,
                );
            }

            let offset = (self.current_frame.slices[slicenum].start_y
                * self.width
                + self.current_frame.slices[slicenum].start_x)
                as isize;
            for y in 0..self.current_frame.slices[slicenum].height as isize {
                // RGB *must* have chroma planes, so this is safe.
                self.decode_line(
                    coder,
                    golomb_coder,
                    //self.current_frame.slices[slicenum],
                    slicenum,
                    frame,
                    self.current_frame.slices[slicenum].width as isize,
                    self.current_frame.slices[slicenum].height as isize,
                    self.width as isize,
                    offset,
                    y,
                    0,
                    0,
                );
                self.decode_line(
                    coder,
                    golomb_coder,
                    //self.current_frame.slices[slicenum],
                    slicenum,
                    frame,
                    self.current_frame.slices[slicenum].width as isize,
                    self.current_frame.slices[slicenum].height as isize,
                    self.width as isize,
                    offset,
                    y,
                    1,
                    1,
                );
                self.decode_line(
                    coder,
                    golomb_coder,
                    //self.current_frame.slices[slicenum],
                    slicenum,
                    frame,
                    self.current_frame.slices[slicenum].width as isize,
                    self.current_frame.slices[slicenum].height as isize,
                    self.width as isize,
                    offset,
                    y,
                    2,
                    1,
                );
                if self.record.extra_plane {
                    self.decode_line(
                        coder,
                        golomb_coder,
                        //self.current_frame.slices[slicenum],
                        slicenum,
                        frame,
                        self.current_frame.slices[slicenum].width as isize,
                        self.current_frame.slices[slicenum].height as isize,
                        self.width as isize,
                        offset,
                        y,
                        3,
                        2,
                    );
                }
            }

            // Convert to RGB all at once, cache locality be damned.
            if self.record.bits_per_raw_sample == 8 {
                rct8(
                    &mut frame.buf,
                    &frame.buf16,
                    self.current_frame.slices[slicenum].width as isize,
                    self.current_frame.slices[slicenum].height as isize,
                    self.width as isize,
                    offset,
                );
            } else if self.record.bits_per_raw_sample >= 9
                && self.record.bits_per_raw_sample <= 15
                && !self.record.extra_plane
            {
                // See: 3.7.2. RGB
                rct_mid(
                    &mut frame.buf16,
                    self.current_frame.slices[slicenum].width as isize,
                    self.current_frame.slices[slicenum].height as isize,
                    self.width as isize,
                    offset,
                    self.record.bits_per_raw_sample as usize,
                );
            } else {
                rct16(
                    &mut frame.buf16,
                    &frame.buf32,
                    self.current_frame.slices[slicenum].width as isize,
                    self.current_frame.slices[slicenum].height as isize,
                    self.width as isize,
                    offset,
                );
            }
        }
    }

    /// Resets the range coder and Golomb-Rice coder states.
    pub fn reset_slice_states(&mut self, slicenum: usize) {
        // Range coder states
        self.current_frame.slices[slicenum].state =
            vec![Vec::new(); self.initial_states.len()];
        for i in 0..self.initial_states.len() {
            self.current_frame.slices[slicenum].state[i] =
                vec![Vec::new(); self.initial_states[i].len()];
            for j in 0..self.initial_states[i].len() {
                self.current_frame.slices[slicenum].state[i][j] =
                    vec![0; self.initial_states[i][j].len()];
                self.current_frame.slices[slicenum].state[i][j]
                    .copy_from_slice(&self.initial_states[i][j]);
            }
        }

        // Golomb-Rice Code states
        if self.record.coder_type == 0 {
            self.current_frame.slices[slicenum].golomb_state =
                vec![Vec::new(); self.record.quant_table_set_count as usize];
            for i in 0..self.current_frame.slices[slicenum].golomb_state.len()
            {
                self.current_frame.slices[slicenum].golomb_state[i] = vec![
                    Default::default();
                    self.record.context_count[i]
                        as usize
                ];
                for j in 0..self.current_frame.slices[slicenum].golomb_state[i]
                    .len()
                {
                    self.current_frame.slices[slicenum].golomb_state[i][j] =
                        State::new();
                }
            }
        }
    }

    pub fn decode_slice(
        &mut self,
        buf: &[u8],
        slicenum: isize,
        frame: &mut Frame,
    ) -> Result<()> {
        // Before we do anything, let's try and check the integrity
        //
        // See: * 4.8.2. error_status
        //      * 4.8.3. slice_crc_parity
        if self.record.ec == 1 {
            if self.current_frame.slice_info[slicenum as usize].error_status
                != 0
            {
                return Err(Error::SliceError(format!(
                    "error_status is non-zero: {}",
                    self.current_frame.slice_info[slicenum as usize]
                        .error_status
                )));
            }

            let slice_buf_first = &buf[self.current_frame.slice_info
                [slicenum as usize]
                .pos as usize..];
            let slice_buf_end =
                &slice_buf_first[..self.current_frame.slice_info
                    [slicenum as usize]
                    .size as usize
                    + 8]; // 8 bytes for footer size
            if crc32_mpeg2(&slice_buf_end) != 0 {
                return Err(Error::InvalidInputData(
                    "CRC mismatch".to_owned(),
                ));
            }
        }

        // If this is a keyframe, refresh states.
        //
        // See: * 3.8.1.3. Initial Values for the Context Model
        //      * 3.8.2.4. Initial Values for the VLC context state
        if self.current_frame.keyframe {
            self.reset_slice_states(slicenum as usize);
        }

        let mut coder = RangeCoder::new(
            &buf[self.current_frame.slice_info[slicenum as usize].pos
                as usize..],
        );

        // 4. Bitstream
        let mut state: [u8; CONTEXT_SIZE as usize] =
            [128; CONTEXT_SIZE as usize];

        // Skip keyframe bit on slice 0
        if slicenum == 0 {
            coder.br(&mut state);
        }

        if self.record.coder_type == 2 {
            // Custom state transition table
            coder.set_table(&self.state_transition);
        }

        self.parse_slice_header(&mut coder, slicenum as usize);

        let mut golomb_coder = if self.record.coder_type == 0 {
            // We're switching to Golomb-Rice mode now so we need the bitstream
            // position.
            //
            // See: 3.8.1.1.1. Termination
            coder.sentinal_end();
            let offset = coder.get_pos() - 1;
            Some(Coder::new(
                &buf[self.current_frame.slice_info[slicenum as usize].pos
                    as usize
                    + offset as usize..],
            ))
        } else {
            None
        };

        // Don't worry, I fully understand how non-idiomatic and
        // ugly passing both c and gc is.
        self.decode_slice_content(
            &mut coder,
            &mut golomb_coder.as_mut(),
            slicenum as usize,
            frame,
        );

        Ok(())
    }
}
