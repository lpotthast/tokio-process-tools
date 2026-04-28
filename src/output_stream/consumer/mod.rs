//! Consumer factories (`inspect_*`, `collect_*`, `wait_for_line*`) and the macro that wires them
//! onto stream backends.

use crate::output_stream::Next;
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use std::borrow::Cow;

#[macro_use]
pub(crate) mod api;
pub(crate) mod collect;
pub(crate) mod inspect;
pub(crate) mod line_waiter;
pub(crate) mod visitor;
pub(crate) mod wait;
pub(crate) mod write;

#[cfg(test)]
mod test_support;

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
