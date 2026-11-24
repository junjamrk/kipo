# GitHub REST v3 Fixture Provenance

All JSON fixtures in this directory conform to GitHub REST API `2022-11-28` (`Accept: application/vnd.github+json`):

1. `repo_private.json`, `repo_public.json`
   - Endpoint: `GET /repos/{owner}/{repo}` (`https://docs.github.com/en/rest/repos/repos#get-a-repository`), fields `id`, `name`, `full_name`, `private`, `visibility`, `owner.login`.
2. `collaborators.json`
   - Endpoint: `GET /repos/{owner}/{repo}/collaborators` (`https://docs.github.com/en/rest/collaborators/collaborators#list-repository-collaborators`), fields `login`, `permissions.{admin, push, pull}`.
3. `issues_page1.json`
   - Endpoint: `GET /repos/{owner}/{repo}/issues?state=all&sort=updated&direction=asc&since=...` (`https://docs.github.com/en/rest/issues/issues#list-repository-issues`), fields `number`, `title`, `body`, `html_url`, `pull_request` (present on PRs), `user.login`, `created_at`, `updated_at`.
