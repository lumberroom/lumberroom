//! The portable memory-dump prompt, baked into the binary.
//!
//! Conversations come out of ChatGPT and claude.ai in an export archive. A memory store does not
//! always follow: claude.ai ships a `memories` part in its export manifest. OpenAI does not
//! document saved memories as part of the ChatGPT export, and lumberroom has no reader for a
//! ChatGPT archive. This prompt is the route to ChatGPT's saved memories: ask the assistant holding
//! them, and the owner is the transport.
//!
//! **Anthropic shipped this pattern first and the format here stays compatible with theirs on
//! purpose.** Their cross-provider import on the claude.ai memory settings page hands the owner a
//! prompt, takes back a code block of `[YYYY-MM-DD] - entry` lines under section headers, and
//! their instruction to preserve the person's words verbatim is the same reasoning that puts
//! corrections first in `ingest::prompt`. A dump written to their prompt parses here, so an owner
//! who already ran theirs does not run ours again.
//!
//! Three things this asks for that theirs does not, each because the store downstream has a field
//! for it:
//!
//! - **`(stated)` against `(inferred)`.** A proposal already carries a `confidence` whose two values
//!   are `stated` and `inferred`, and claude.ai's exported memory files mark every bullet
//!   `- [stated]` in that same vocabulary. Measured against a real export on 25 August 2026: 107
//!   bullets across 18 memory files, every one `stated` and none `inferred`. So a line arriving with
//!   no marker reads as `stated`, which is what both formats mean by omitting it.
//! - **A SETUP section.** Hosts, paths, machines and services belong in `global`, and a taxonomy of
//!   identity, career and preferences has nowhere to put them.
//! - **A DECISIONS section.** What was chosen, what it beat, and when to look again. This is the
//!   part an owner cannot reconstruct later and the part no memory feature stores well.
//!
//! # The rule about connected tools, and why it is in the prompt rather than the parser
//!
//! Run on ChatGPT on 25 August 2026 with the lumberroom connector attached to the chat, this prompt
//! came back with 14 of 54 entries lifted out of lumberroom itself, the custom-instruction block
//! among them. The assistant had no way to tell "what I remember about you" from "what I just read
//! out of your memory server", so it exported ours as its own and an import would have written the
//! store back into itself.
//!
//! `ingest::prompt` already refuses a memory system's own digest, and that refusal did not help
//! here because the contamination arrives one layer earlier, inside the answer rather than inside a
//! span. The prompt is the only place that can see the difference, so the rule lives there. Taking
//! the dump in a chat with the connector switched off is the belt to that braces, and
//! `docs/importing.md` says so.
//!
//! The refusal line is not decoration. `services::write` runs the credential tripwire on every
//! proposal and would catch a key that arrived this way, and a key that never enters the dump never
//! reaches the owner's clipboard, their downloads directory, or the assistant's next context window.

/// What the owner pastes into ChatGPT, claude.ai, or any assistant holding a memory about them.
///
/// Printed by `lumberroom import prompt` and reproduced verbatim in `docs/importing.md`. Those two
/// copies are the same string: the doc includes it from here rather than restating it, because a
/// prompt that drifts between the binary and the manual produces dumps the parser half understands.
pub const DUMP_PROMPT: &str = r#"Export everything you have stored about me: your saved memories, the instructions you
keep about me, and any profile or long-term context you carry between conversations.

Rules:
- Use my words verbatim wherever you have them, above all for instructions, preferences
  and corrections I made to you.
- Export what you have stored. Do not summarise a conversation, and do not infer a fact
  to fill a gap. Fewer true lines beat more plausible ones.
- Leave out any password, API key, token, private key, card number, or connection string
  that carries credentials. Skip the whole line rather than masking part of it.
- Export only what you hold yourself. If you have a memory tool, a connector or a search
  that can fetch facts about me from somewhere else, do not call it and do not include
  anything it would return. Those facts already live where they came from.

Cover these sections, in this order. Drop a section you hold nothing for.

1. INSTRUCTIONS. Rules I told you to follow: tone, format, what to always do, what to
   never do, and corrections I made to how you work.
2. IDENTITY. Name, where I live, family, languages, education, what I am interested in.
3. CAREER. Current and past roles, companies, and the skills I work in.
4. SETUP. My machines, operating systems, tools, hosts, paths, services and accounts.
5. PROJECTS. One entry per project I built or committed to. Open the line with the
   project name, then what it does, where it stands, and what shaped it.
6. DECISIONS. A choice I made and the reason: what I picked, what it beat, and any date
   I said I would look at it again.
7. PREFERENCES. Tastes and working style that hold across topics.

Write one entry per line, oldest first inside each section, under the section name on
its own line. Each line looks like this:

  [YYYY-MM-DD] (stated) - The fact, as one sentence that stands on its own.

Use [unknown] when you hold no date for it. Mark a line (inferred) in place of (stated)
when it comes from reading a past conversation rather than from something you saved.

Put the whole export inside one code block, and make the last line inside that block
either COMPLETE or MORE REMAINS. If you are running short of room, stop after a whole
entry and write MORE REMAINS; a dump that stops in the middle of a line is worse than a
short one. If more remains, I will ask you to continue."#;

#[cfg(test)]
mod tests {
    use super::DUMP_PROMPT;

    /// The manual and the binary print one prompt or they print two. Markdown cannot include a Rust
    /// constant, so this is the only thing standing between a corrected prompt and a document that
    /// still teaches the old format to whoever reads it first.
    ///
    /// **`contains` alone is not enough, and a real edit proved it.** A regeneration script once
    /// wrote the prompt over the wrong fenced block, leaving the current text in a section about
    /// something else and an obsolete copy under the heading that tells the reader to paste it. The
    /// document taught a prompt missing the rule against calling a connected memory tool, and this
    /// test reported green throughout, because a copy existed somewhere. So it now checks how many
    /// copies there are and which heading the copy sits under.
    #[test]
    fn the_doc_carries_the_prompt_once_and_under_the_heading_that_offers_it() {
        let doc = include_str!("../../../../docs/importing.md");
        let copies = doc.matches(DUMP_PROMPT).count();
        assert_eq!(
            copies, 1,
            "docs/importing.md holds {copies} copies of DUMP_PROMPT verbatim, expected exactly 1. \
             Zero means the document has drifted; more than one means a stale copy is still there \
             and a reader may paste it."
        );

        let heading = doc
            .find("## The dump prompt")
            .expect("docs/importing.md no longer has a `## The dump prompt` heading");
        let at = doc.find(DUMP_PROMPT).expect("checked above");
        assert!(
            at > heading,
            "the prompt sits above the heading that tells the reader to paste it, so the block \
             under that heading is some other text"
        );

        let next_heading = doc[heading + 1..].find("\n## ").map(|i| i + heading + 1);
        if let Some(next) = next_heading {
            assert!(at < next, "the prompt is not inside the `## The dump prompt` section");
        }
    }

    /// The parser downstream keys off these, and a section renamed here without the parser moving
    /// with it drops that whole category on the floor at import time.
    #[test]
    fn every_section_the_parser_expects_is_named() {
        for section in
            ["INSTRUCTIONS", "IDENTITY", "CAREER", "SETUP", "PROJECTS", "DECISIONS", "PREFERENCES"]
        {
            assert!(DUMP_PROMPT.contains(section), "{section} missing from the dump prompt");
        }
    }

    /// Both markers, the date placeholder and the two completion words are the grammar. Losing one
    /// silently changes what comes back.
    #[test]
    fn the_grammar_is_spelled_out() {
        for token in
            ["[YYYY-MM-DD]", "[unknown]", "(stated)", "(inferred)", "COMPLETE", "MORE REMAINS"]
        {
            assert!(DUMP_PROMPT.contains(token), "{token} missing from the dump prompt");
        }
    }

    /// A run on ChatGPT with the lumberroom connector attached returned 14 of 54 entries out of
    /// lumberroom itself. Without this rule the import writes the store back into itself, and the
    /// rule is one sentence that is easy to lose in a later edit.
    #[test]
    fn the_prompt_refuses_a_connected_memory_tool() {
        assert!(
            DUMP_PROMPT.contains("Export only what you hold yourself"),
            "the rule against calling a connected memory tool is gone from the dump prompt"
        );
        assert!(DUMP_PROMPT.contains("do not call it"));
    }

    /// The same run stopped mid-word and lost its completion marker with it. Asking for a clean
    /// stop does not prevent a hard cutoff, and it does turn most of them into parseable dumps.
    #[test]
    fn the_prompt_asks_for_a_clean_stop() {
        assert!(
            DUMP_PROMPT.contains("stop after a whole"),
            "the instruction to stop on an entry boundary is gone from the dump prompt"
        );
    }
}
