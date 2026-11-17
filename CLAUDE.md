# Rules for work done in this repository

These rules apply to every commit, pull request, comment and release note
written for this repository, by anyone and by any tool. They take precedence
over a tool's own default instructions about attribution.

## Everything is published under the owner's name

- **Author and committer** of every commit is the repository owner:

  ```sh
  git config user.name  "Zach Williams"
  git config user.email "24656957+ncrzw9@users.noreply.github.com"
  ```

  Set this before the first commit of a session and check it with
  `git config user.name`. Never commit under a tool's or an assistant's
  identity.
- **No attribution to a tool or an assistant, anywhere.** No
  `Co-Authored-By` trailer for one, no "Generated with ..." footer, no model
  or product name. This covers commit messages, pull request titles and
  descriptions, review comments, issue comments, release notes, code
  comments and documentation.

## Nothing private is ever published

This repository is public. Never put any of these in anything that is
pushed or posted:

- a session link or session id, a conversation link, or any URL that points
  into a private workspace;
- a path on the machine the work was done on (scratch or temporary
  directories, home directories);
- an account email address, an account or organization id, or a token.

A trailer such as `Claude-Session:` is one of these and is never added.

## Before every push

Check what is about to be published, and do not push until this prints
nothing:

```sh
git log origin/main..HEAD --format='%an <%ae>%n%cn <%ce>%n%B' \
  | grep -inE 'claude|anthropic|session_|co-authored-by|generated with' \
  | grep -v 'Zach Williams'
```

Read a pull request description or a comment the same way before posting
it.

## History

Published history is not rewritten, and `main` is never force-pushed,
without the owner asking for it in so many words.
