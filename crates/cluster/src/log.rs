//! Bounded committed-log polling with Aeron's own fragment assembly.
//!
//! Assembly survives polls, but an incomplete or malformed fragment sequence must never be
//! mistaken for padding when a log image finishes: doing so could acknowledge a skipped command.

use std::cell::Cell;

use rusteron_client::{
    AeronCError, AeronFragmentAssembler, AeronFragmentClosureAssembler,
    AeronFragmentHandlerCallback, AeronHeader, AeronImage, FragmentAssemblable, Handler,
};

#[derive(Clone, Copy, Default)]
struct FragmentProgress {
    pending: bool,
    invalid: Option<&'static str>,
}

impl FragmentProgress {
    fn observe(&mut self, flags: u8) -> bool {
        if self.invalid.is_some() {
            return false;
        }
        let starts = flags & 0x80 != 0;
        if starts && self.pending {
            self.invalid =
                Some("a new message started before the prior fragmented message completed");
        } else if !starts && !self.pending {
            self.invalid = Some("a log image contains an orphan continuation/end fragment");
        } else {
            // Even an END or unfragmented frame stays pending until the native assembler's
            // complete-message delegate actually runs; native assembly can reject a gap.
            self.pending = true;
        }
        self.invalid.is_none()
    }

    fn complete(&mut self) {
        if self.invalid.is_none() {
            self.pending = false;
        }
    }

    fn finish_position(self, applied: i64, image: i64) -> Result<i64, &'static str> {
        if let Some(reason) = self.invalid {
            return Err(reason);
        }
        if self.pending {
            return Err("log image ended inside a fragmented message");
        }
        // Aeron may have consumed term padding after the last complete message.
        Ok(applied.max(image))
    }
}

trait RawLogSource {
    fn poll_raw(
        &self,
        handler: impl FnMut(&[u8], AeronHeader),
        fragment_limit: usize,
    ) -> Result<i32, AeronCError>;
}

struct BoundedImage<'a> {
    image: &'a AeronImage,
    limit_position: i64,
}

impl RawLogSource for BoundedImage<'_> {
    fn poll_raw(
        &self,
        handler: impl FnMut(&[u8], AeronHeader),
        fragment_limit: usize,
    ) -> Result<i32, AeronCError> {
        self.image
            .bounded_poll_fn(handler, self.limit_position, fragment_limit)
    }
}

struct TrackedSource<'a, P> {
    source: &'a P,
    progress: &'a Cell<FragmentProgress>,
}

impl<P: RawLogSource> FragmentAssemblable for TrackedSource<'_, P> {
    fn poll_with_assembler(
        &self,
        assembler: Option<&Handler<AeronFragmentAssembler>>,
        fragment_limit: usize,
    ) -> Result<i32, AeronCError> {
        let Some(mut assembler) = assembler.cloned() else {
            return Ok(0);
        };
        let result = self.source.poll_raw(
            |buffer, header| {
                let mut progress = self.progress.get();
                match header.get_values() {
                    Ok(values) => {
                        progress.observe(values.frame().flags());
                    }
                    Err(_) => {
                        progress.invalid = Some("cannot read a committed-log fragment header");
                    }
                }
                self.progress.set(progress);
                if progress.invalid.is_none() {
                    // Handler implements the callback trait: no borrowed raw callback context or
                    // guessed header layout is introduced here. Calls stay on the service thread.
                    assembler.handle_aeron_fragment_handler(buffer, header);
                }
            },
            fragment_limit,
        );
        // The native assembler catches delegate panics; raise them while the closure assembler
        // still owns its borrowed delegate context and can clear it during unwinding.
        rusteron_client::resume_caught_panic();
        result
    }
}

pub(crate) struct LogAssembler {
    inner: AeronFragmentClosureAssembler,
    progress: Cell<FragmentProgress>,
}

impl LogAssembler {
    pub(crate) fn new() -> Result<Self, AeronCError> {
        Ok(Self {
            inner: AeronFragmentClosureAssembler::new()?,
            progress: Cell::default(),
        })
    }

    fn poll_source<P: RawLogSource, T>(
        &mut self,
        source: &P,
        fragment_limit: usize,
        context: &mut T,
        on_message: fn(&mut T, &[u8], AeronHeader),
    ) -> Result<i32, String> {
        let mut delegate = (context, on_message, &self.progress);
        let read = self
            .inner
            .poll(
                &TrackedSource {
                    source,
                    progress: &self.progress,
                },
                &mut delegate,
                |delegate, buffer, header| {
                    let mut progress = delegate.2.get();
                    progress.complete();
                    delegate.2.set(progress);
                    (delegate.1)(delegate.0, buffer, header);
                },
                fragment_limit,
            )
            .map_err(|error| format!("committed-log poll failed: {error:?}"))?;
        if let Some(reason) = self.progress.get().invalid {
            return Err(reason.to_string());
        }
        Ok(read)
    }

    pub(crate) fn poll<T>(
        &mut self,
        image: &AeronImage,
        limit_position: i64,
        fragment_limit: usize,
        context: &mut T,
        on_message: fn(&mut T, &[u8], AeronHeader),
    ) -> Result<i32, String> {
        self.poll_source(
            &BoundedImage {
                image,
                limit_position,
            },
            fragment_limit,
            context,
            on_message,
        )
    }

    /// Must succeed before dropping an image or acknowledging its padding-adjusted position.
    pub(crate) fn finish_position(&self, applied: i64, image: i64) -> Result<i64, &'static str> {
        self.progress.get().finish_position(applied, image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusteron_client::bindings::{aeron_data_header_t, aeron_frame_header_t, aeron_header_t};
    use std::{cell::Cell, ffi::c_void};

    // aeron_header_t is opaque in bindgen. This test-only layout is copied from the pinned
    // Aeron 1.54.0 aeron-client/src/main/c/aeron_image.h, not used in production code.
    #[repr(C)]
    struct TestHeader {
        frame: *mut aeron_data_header_t,
        initial_term_id: i32,
        position_bits_to_shift: usize,
        fragmented_frame_length: i32,
        context: *mut c_void,
    }

    struct Fragment {
        bytes: Vec<u8>,
        flags: u8,
        offset: i32,
    }

    impl Fragment {
        fn end(&self) -> i64 {
            i64::from(self.offset) + (32 + self.bytes.len()).next_multiple_of(32) as i64
        }
    }

    struct Fragments {
        frames: Vec<Fragment>,
        next: Cell<usize>,
        committed: Cell<i64>,
    }

    impl RawLogSource for Fragments {
        fn poll_raw(
            &self,
            mut handler: impl FnMut(&[u8], AeronHeader),
            fragment_limit: usize,
        ) -> Result<i32, AeronCError> {
            let mut count = 0;
            while count < fragment_limit {
                let Some(fragment) = self.frames.get(self.next.get()) else {
                    break;
                };
                if fragment.end() > self.committed.get() {
                    break;
                }
                let mut frame = aeron_data_header_t {
                    frame_header: aeron_frame_header_t {
                        frame_length: (32 + fragment.bytes.len()) as i32,
                        version: 0,
                        flags: fragment.flags,
                        type_: 1,
                    },
                    term_offset: fragment.offset,
                    session_id: 42,
                    stream_id: 100,
                    term_id: 0,
                    reserved_value: 0,
                };
                assert_eq!(std::mem::size_of::<aeron_data_header_t>(), 32);
                // Assert the pinned 64-bit Aeron header ABI before passing its address to C.
                #[cfg(target_pointer_width = "64")]
                {
                    assert_eq!(std::mem::size_of::<TestHeader>(), 40);
                    assert_eq!(std::mem::offset_of!(TestHeader, initial_term_id), 8);
                    assert_eq!(std::mem::offset_of!(TestHeader, position_bits_to_shift), 16);
                    assert_eq!(
                        std::mem::offset_of!(TestHeader, fragmented_frame_length),
                        24
                    );
                    assert_eq!(std::mem::offset_of!(TestHeader, context), 32);
                }
                let mut raw = TestHeader {
                    frame: &mut frame,
                    initial_term_id: 0,
                    position_bits_to_shift: 16,
                    fragmented_frame_length: 0,
                    context: std::ptr::null_mut(),
                };
                // SAFETY: the layout matches the pinned C header above; both stack objects and
                // the payload outlive this synchronous handler call. No aliasing/reentrant poll.
                {
                    handler(
                        &fragment.bytes,
                        AeronHeader::from((&mut raw as *mut TestHeader).cast::<aeron_header_t>()),
                    );
                }
                self.next.set(self.next.get() + 1);
                count += 1;
            }
            Ok(count as i32)
        }
    }

    fn record(messages: &mut Vec<(Vec<u8>, i64)>, bytes: &[u8], header: AeronHeader) {
        messages.push((bytes.to_vec(), header.position()));
    }

    fn fragment(bytes: &[u8], max_payload: usize) -> Fragments {
        assert!(bytes.len() > max_payload);
        assert_eq!((32 + max_payload) % 32, 0, "MTU must be frame aligned");
        let chunks = bytes.len().div_ceil(max_payload);
        let mut offset = 0;
        let frames = bytes
            .chunks(max_payload)
            .enumerate()
            .map(|(index, bytes)| {
                let flags = if index == 0 {
                    0x80
                } else if index + 1 == chunks {
                    0x40
                } else {
                    0
                };
                let frame = Fragment {
                    bytes: bytes.to_vec(),
                    flags,
                    offset,
                };
                offset = frame.end() as i32;
                frame
            })
            .collect();
        Fragments {
            frames,
            next: Cell::new(0),
            committed: Cell::new(i64::MAX),
        }
    }

    #[test]
    fn mtu_sized_fragments_reassemble_once_across_polls_at_the_final_position() {
        // Aeron infers the MTU from the begin frame when calculating the assembled header's
        // final position (aeron_fragment_assembler.h:108-119). All non-final payloads must
        // therefore be that size. Sweep every final payload length over several aligned MTUs,
        // including a realistic 1408-byte MTU, and always include a middle fragment.
        for max_payload in [32, 64, 96, 224, 480, 1376] {
            for tail in 1..=max_payload {
                let bytes: Vec<u8> = (0..2 * max_payload + tail)
                    .map(|index| index as u8)
                    .collect();
                let source = fragment(&bytes, max_payload);
                assert_eq!(source.frames.len(), 3);
                let mut assembler = LogAssembler::new().expect("native assembler");
                let mut received = Vec::new();
                for index in 0..source.frames.len() {
                    assert_eq!(
                        assembler
                            .poll_source(&source, 1, &mut received, record)
                            .expect("fragment"),
                        1
                    );
                    if index + 1 < source.frames.len() {
                        assert!(
                            received.is_empty(),
                            "partial message: MTU payload {max_payload}, tail {tail}"
                        );
                    }
                }
                assert_eq!(
                    received,
                    vec![(bytes, source.frames.last().expect("end frame").end())],
                    "MTU payload {max_payload}, tail {tail}"
                );
            }
        }
    }

    #[test]
    fn uncommitted_tail_waits_for_commit_before_delivery() {
        let bytes = vec![7; 97];
        let source = fragment(&bytes, 32);
        source.committed.set(source.frames[0].end());
        let mut assembler = LogAssembler::new().expect("native assembler");
        let mut received = Vec::new();
        assert_eq!(
            assembler
                .poll_source(&source, 256, &mut received, record)
                .expect("bounded begin"),
            1
        );
        assert!(received.is_empty());
        assert_eq!(
            assembler
                .poll_source(&source, 256, &mut received, record)
                .expect("still bounded"),
            0
        );
        assert!(received.is_empty());
        source.committed.set(i64::MAX);
        assert_eq!(
            assembler
                .poll_source(&source, 256, &mut received, record)
                .expect("committed tail"),
            3
        );
        assert_eq!(
            received,
            vec![(bytes, source.frames.last().expect("end frame").end())]
        );
    }

    #[test]
    fn image_replacement_cannot_complete_an_old_begin() {
        let source = fragment(&[7; 97], 32);
        let mut assembler = LogAssembler::new().expect("native assembler");
        let mut received = Vec::new();
        assert_eq!(
            assembler
                .poll_source(&source, 1, &mut received, record)
                .expect("begin"),
            1
        );
        assert!(received.is_empty());
        assert!(
            assembler
                .finish_position(0, source.frames[0].end())
                .is_err()
        );
        // Even if the caller incorrectly replaced this unfinished image, its orphan tail must
        // refuse rather than masquerading as padding on a fresh assembler.
        assembler = LogAssembler::new().expect("new image assembler");
        assert!(
            assembler
                .poll_source(&source, 256, &mut received, record)
                .is_err()
        );
        assert!(
            assembler
                .finish_position(0, source.frames.last().expect("tail").end())
                .is_err()
        );
        assert!(received.is_empty());
    }

    #[test]
    fn finish_guard_refuses_begin_and_middle_but_accepts_complete_messages_and_padding() {
        let bytes = vec![7; 97];
        let source = fragment(&bytes, 32);
        let mut assembler = LogAssembler::new().expect("native assembler");
        // A padding-only image needs no callback and is safe to advance.
        assert_eq!(assembler.finish_position(0, 96), Ok(96));
        let mut received = Vec::new();
        for index in 0..source.frames.len() {
            assembler
                .poll_source(&source, 1, &mut received, record)
                .expect("fragment");
            let raw_position = source.frames[index].end();
            if index + 1 < source.frames.len() {
                assert!(
                    assembler.finish_position(0, raw_position).is_err(),
                    "partial {index}"
                );
                assert!(received.is_empty());
            } else {
                assert_eq!(
                    assembler.finish_position(raw_position, raw_position + 96),
                    Ok(raw_position + 96)
                );
                assert_eq!(received, vec![(bytes.clone(), raw_position)]);
            }
        }
    }

    #[test]
    fn abandoning_a_partial_message_is_permanently_invalid_even_after_later_completion() {
        for flags in [0x80, 0xc0] {
            let mut source = fragment(&[7; 97], 32);
            source.frames[1].flags = flags;
            let mut assembler = LogAssembler::new().expect("native assembler");
            let mut received = Vec::new();
            assert!(
                assembler
                    .poll_source(&source, 256, &mut received, record)
                    .is_err()
            );
            assert!(received.is_empty());
            assert!(
                assembler
                    .finish_position(0, source.frames.last().expect("end").end())
                    .is_err()
            );
            // A delegate completion cannot erase evidence of the prior abandoned begin.
            let mut progress = assembler.progress.get();
            progress.complete();
            assert!(progress.finish_position(0, 1024).is_err());
        }
    }

    #[test]
    fn orphan_middle_and_end_fragments_are_not_clean_padding() {
        for flags in [0, 0x40] {
            let source = Fragments {
                frames: vec![Fragment {
                    bytes: vec![7; 32],
                    flags,
                    offset: 0,
                }],
                next: Cell::new(0),
                committed: Cell::new(i64::MAX),
            };
            let mut assembler = LogAssembler::new().expect("native assembler");
            let mut received = Vec::new();
            assert!(
                assembler
                    .poll_source(&source, 256, &mut received, record)
                    .is_err()
            );
            assert!(assembler.finish_position(0, 64).is_err());
            assert!(received.is_empty());
        }
    }

    #[test]
    fn an_end_fragment_ignored_by_native_assembly_does_not_clear_pending_state() {
        let mut source = fragment(&[7; 97], 32);
        source.frames.last_mut().expect("end").offset += 32;
        let mut assembler = LogAssembler::new().expect("native assembler");
        let mut received = Vec::new();
        assembler
            .poll_source(&source, 256, &mut received, record)
            .expect("native poll");
        assert!(
            received.is_empty(),
            "a discontinuous tail must not dispatch"
        );
        assert!(
            assembler
                .finish_position(0, source.frames.last().expect("end").end())
                .is_err()
        );
    }

    #[test]
    fn unfragmented_message_is_delivered_without_waiting_for_another_fragment() {
        let source = Fragments {
            frames: vec![Fragment {
                bytes: b"complete".to_vec(),
                flags: 0xc0,
                offset: 0,
            }],
            next: Cell::new(0),
            committed: Cell::new(i64::MAX),
        };
        let mut assembler = LogAssembler::new().expect("native assembler");
        let mut received = Vec::new();
        assembler
            .poll_source(&source, 256, &mut received, record)
            .expect("unfragmented");
        assert_eq!(
            received,
            vec![(b"complete".to_vec(), source.frames[0].end())]
        );
    }
}
