#[cfg(feature = "skills")]
pub(crate) use adk_skill::{SelectionPolicy, SkillIndex, apply_skill_injection, load_skill_index};

#[cfg(all(feature = "skills", feature = "codeact"))]
pub(crate) use adk_skill::select_skill_prompt_block;

#[cfg(not(feature = "skills"))]
mod disabled {
    use adk_core::Content;

    #[derive(Debug, Clone, Default)]
    pub(crate) struct SelectionPolicy {
        _disabled: (),
    }

    #[derive(Debug, Clone, Default)]
    pub(crate) struct SkillIndex {
        _disabled: (),
    }

    #[cfg(feature = "codeact")]
    pub(crate) fn select_skill_prompt_block(
        _index: &SkillIndex,
        _query: &str,
        _policy: &SelectionPolicy,
        _max_injected_chars: usize,
    ) -> Option<((), String)> {
        None
    }

    pub(crate) fn apply_skill_injection(
        _content: &mut Content,
        _index: &SkillIndex,
        _policy: &SelectionPolicy,
        _max_injected_chars: usize,
    ) -> Option<()> {
        None
    }
}

#[cfg(not(feature = "skills"))]
pub(crate) use disabled::{SelectionPolicy, SkillIndex, apply_skill_injection};

#[cfg(all(not(feature = "skills"), feature = "codeact"))]
pub(crate) use disabled::select_skill_prompt_block;
