//! Detects publicly disclosed action vulnerabilities.
//!
//! This audit uses GitHub's security advisories API as a source of
//! ground truth.
//!
//! See: <https://docs.github.com/en/rest/security-advisories/global-advisories?apiVersion=2022-11-28>

use anyhow::{anyhow, Result};
use github_actions_models::workflow::{job::StepBody, Job};

use crate::{
    finding::{Confidence, Severity},
    github_api,
    models::Uses,
    AuditConfig,
};

use super::WorkflowAudit;

pub(crate) struct KnownVulnerableActions<'a> {
    pub(crate) _config: AuditConfig<'a>,
    client: github_api::Client,
}

impl<'a> KnownVulnerableActions<'a> {
    fn action_known_vulnerabilities(&self, uses: &Uses<'_>) -> Result<Vec<(Severity, String)>> {
        let version = match uses.git_ref {
            // If `uses` is pinned to a symbolic ref, we need to perform
            // feats of heroism to figure out what's going on.
            // In the "happy" case the symbolic ref is an exact version tag,
            // which we can then query directly for.
            // Besides that, there are two unhappy cases:
            // 1. The ref is a "version", but it's something like a "v3"
            //    branch or tag. These are obnoxious to handle, but we
            //    can do so with a heuristic: resolve the ref to a commit,
            //    then find the longest tag name that also matches that commit.
            //    For example, branch `v1` becomes tag `v1.2.3`.
            // 2. The ref is something version-y but not itself a version,
            //    like `gh-action-pypi-publish`'s `release/v1` branch.
            //    We use the same heuristic for these.
            //
            // To handle all of the above, we convert the ref into a commit
            // and then find the longest tag for that commit.
            Some(version) if !uses.ref_is_commit() => {
                let Some(commit_ref) =
                    self.client.commit_for_ref(uses.owner, uses.repo, version)?
                else {
                    // No `ref -> commit` means that the action's version
                    // is probably just outright invalid.
                    return Ok(vec![]);
                };

                match self
                    .client
                    .longest_tag_for_commit(uses.owner, uses.repo, &commit_ref)?
                {
                    Some(tag) => tag.name,
                    // Somehow we've round-tripped through a commit and ended
                    // up without a tag, which suggests we went
                    // `branch -> sha -> {no tag}`. In that case just use our
                    // original ref, since it's the best we have.
                    None => version.to_string(),
                }
            }
            // If `uses` is pinned to a sha-ref, we need to find the
            // tag matching that ref. In theory the action's repo could do
            // something annoying like use branches for versions instead,
            // which we should also probably support.
            Some(commit_ref) => match self
                .client
                .longest_tag_for_commit(uses.owner, uses.repo, commit_ref)?
            {
                Some(tag) => tag.name,
                // No corresponding tag means the user is maybe doing something
                // weird, like using a commit ref off of a branch that isn't
                // also tagged. Probably not good, but also not something
                // we can easily discover known vulns for.
                None => return Ok(vec![]),
            },
            // No version means the action runs the latest default branch
            // version. We could in theory query GHSA for this but it's
            // unlikely to be meaningful.
            // TODO: Maybe we need a separate (low-sev) audit for actions usage
            // on @master/@main/etc?
            None => return Ok(vec![]),
        };

        let vulns = self
            .client
            .gha_advisories(uses.owner, uses.repo, &version)?;

        let mut results = vec![];

        for vuln in vulns {
            let severity = match vuln.severity.as_str() {
                "low" => Severity::Unknown,
                "medium" => Severity::Medium,
                "high" => Severity::High,
                "critical" => Severity::High,
                _ => Severity::Unknown,
            };

            results.push((severity, vuln.ghsa_id));
        }

        Ok(results)
    }
}

impl<'a> WorkflowAudit<'a> for KnownVulnerableActions<'a> {
    fn ident() -> &'static str
    where
        Self: Sized,
    {
        "known-vulnerable-actions"
    }

    fn desc() -> &'static str
    where
        Self: Sized,
    {
        "action has a known vulnerability"
    }

    fn new(config: AuditConfig<'a>) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        if config.offline {
            return Err(anyhow!("offline audits only requested"));
        }

        let Some(gh_token) = config.gh_token else {
            return Err(anyhow!("can't audit without a GitHub API token"));
        };

        let client = github_api::Client::new(gh_token);

        Ok(Self {
            _config: config,
            client,
        })
    }

    fn audit<'w>(
        &mut self,
        workflow: &'w crate::models::Workflow,
    ) -> anyhow::Result<Vec<crate::finding::Finding<'w>>> {
        let mut findings = vec![];

        for job in workflow.jobs() {
            let Job::NormalJob(_) = *job else {
                continue;
            };

            for step in job.steps() {
                let StepBody::Uses { uses, .. } = &step.body else {
                    continue;
                };

                let Some(uses) = Uses::from_step(uses) else {
                    continue;
                };

                for (severity, id) in self.action_known_vulnerabilities(&uses)? {
                    findings.push(
                        Self::finding()
                            .confidence(Confidence::High)
                            .severity(severity)
                            .add_location(step.location().with_keys(&["uses".into()]).annotated(id))
                            .build(workflow)?,
                    );
                }
            }
        }

        Ok(findings)
    }
}