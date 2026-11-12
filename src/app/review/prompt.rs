use std::path::Path;

use super::PullRequest;

// A choice's prompt, in which only the named fields are replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prompt(Vec<Part>);

pub(crate) struct PromptData<'data> {
    pub(crate) pr: &'data PullRequest,
    pub(crate) requester: Option<&'data str>,
    pub(crate) skill: &'data str,
    pub(crate) skill_path: &'data Path,
    pub(crate) worktree: &'data Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Field(Field),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Requester,
    Skill,
    SkillPath,
    Worktree,
    Repo,
    Number,
    Title,
    Url,
    HeadSha,
    BaseRef,
}

const FIELDS: [(&str, Field); 10] = [
    (".Requester", Field::Requester),
    (".Skill", Field::Skill),
    (".SkillPath", Field::SkillPath),
    (".Worktree", Field::Worktree),
    (".PR.Repo", Field::Repo),
    (".PR.Number", Field::Number),
    (".PR.Title", Field::Title),
    (".PR.URL", Field::Url),
    (".PR.HeadSHA", Field::HeadSha),
    (".PR.BaseRef", Field::BaseRef),
];

impl Prompt {
    // Spaces inside the braces are allowed, since prompts written for Go templates use both forms.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find("{{") {
            if open > 0 {
                parts.push(Part::Text(rest[..open].to_owned()));
            }
            let action = &rest[open + 2..];
            let Some(close) = action.find("}}") else {
                return Err(format!("{{{{ at {:?} has no closing }}}}", &rest[open..]));
            };
            let name = action[..close].trim_matches(' ');
            let Some((_, field)) = FIELDS.iter().find(|(known, _)| *known == name) else {
                return Err(format!(
                    "{{{{{}}}}} is not a field; a prompt can name only {}",
                    &action[..close],
                    field_list()
                ));
            };
            parts.push(Part::Field(*field));
            rest = &action[close + 2..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_owned()));
        }
        Ok(Self(parts))
    }

    #[must_use]
    pub(crate) fn render(&self, data: &PromptData<'_>) -> String {
        self.0
            .iter()
            .map(|part| match part {
                Part::Text(text) => text.clone(),
                Part::Field(field) => field.value(data),
            })
            .collect()
    }
}

impl Field {
    fn value(self, data: &PromptData<'_>) -> String {
        match self {
            Self::Requester => data.requester.unwrap_or_default().to_owned(),
            Self::Skill => data.skill.to_owned(),
            Self::SkillPath => data.skill_path.to_string_lossy().into_owned(),
            Self::Worktree => data.worktree.to_string_lossy().into_owned(),
            Self::Repo => data.pr.repo.clone(),
            Self::Number => data.pr.number.to_string(),
            Self::Title => data.pr.title.clone(),
            Self::Url => data.pr.url.clone(),
            Self::HeadSha => data.pr.head_sha.clone(),
            Self::BaseRef => data.pr.base_ref.clone(),
        }
    }
}

fn field_list() -> String {
    FIELDS
        .iter()
        .map(|(name, _)| format!("{{{{{name}}}}}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_is_replaced_and_the_rest_kept() {
        let prompt = Prompt::parse(
            "[${{.Skill}}]({{.SkillPath}}) {{.PR.Repo}}#{{ .PR.Number }} {{.PR.Title}}\n{{.PR.URL}} {{.PR.HeadSHA}} {{.PR.BaseRef}} {{.Worktree}} @{{.Requester}} }} done",
        )
        .expect("a valid prompt");
        let pr = PullRequest {
            repo: "acme/app".to_owned(),
            number: 12,
            title: "Fix".to_owned(),
            url: "https://github.com/acme/app/pull/12".to_owned(),
            head_sha: "abc".to_owned(),
            base_ref: "main".to_owned(),
        };

        let rendered = prompt.render(&PromptData {
            pr: &pr,
            requester: Some("tanaka"),
            skill: "code-review",
            skill_path: Path::new("/c/SKILL.md"),
            worktree: Path::new("/w/pr-12"),
        });

        assert_eq!(
            rendered,
            "[$code-review](/c/SKILL.md) acme/app#12 Fix\nhttps://github.com/acme/app/pull/12 abc main /w/pr-12 @tanaka }} done"
        );
    }

    #[test]
    fn unknown_requester_is_left_empty() {
        let prompt = Prompt::parse("by {{.Requester}}.").expect("a valid prompt");
        let pr = PullRequest {
            repo: "acme/app".to_owned(),
            number: 1,
            title: String::new(),
            url: String::new(),
            head_sha: String::new(),
            base_ref: String::new(),
        };

        let rendered = prompt.render(&PromptData {
            pr: &pr,
            requester: None,
            skill: "s",
            skill_path: Path::new("/s"),
            worktree: Path::new("/w"),
        });

        assert_eq!(rendered, "by .");
    }

    #[test]
    fn go_template_beyond_field_replacement_is_refused() {
        for (name, text) in [
            ("unknown_field", "{{.PR.Nope}}"),
            ("condition", "{{if .Requester}}x{{end}}"),
            ("pipeline", "{{.PR.Title | printf \"%q\"}}"),
            ("trim_marker", "a {{- .Skill}}"),
            ("comment", "{{/* note */}}"),
            ("unclosed", "{{.Skill"),
            ("lower_case", "{{.pr.url}}"),
        ] {
            let parsed = Prompt::parse(text);

            assert!(parsed.is_err(), "case: {name}: {parsed:?}");
        }
    }
}
