use crate::output_stream::{LineParserState, LineParsingOptions, Next};
use std::borrow::Cow;

pub(crate) mod api;
pub(crate) mod collect;
pub(crate) mod inspect;
pub(crate) mod wait;
pub(crate) mod write;

pub(crate) fn visit_lines(
    chunk: &[u8],
    parser: &mut LineParserState,
    options: LineParsingOptions,
    f: impl FnMut(Cow<'_, str>) -> Next,
) -> Next {
    parser.visit_chunk(chunk, options, f)
}

pub(crate) fn visit_final_line(
    parser: &LineParserState,
    f: impl FnOnce(Cow<'_, str>) -> Next,
) -> Next {
    parser.finish(f)
}

pub(crate) fn collect_owned_final_line(parser: &LineParserState) -> Option<String> {
    parser.finish_owned()
}
