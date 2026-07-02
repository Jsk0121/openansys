const repoLink = document.querySelector("#repo-link");

if (repoLink && location.hostname.endsWith("github.io")) {
  const parts = location.pathname.split("/").filter(Boolean);
  const owner = location.hostname.replace(".github.io", "");
  const repo = parts[0];

  if (owner && repo) {
    repoLink.href = `https://github.com/${owner}/${repo}`;
  }
}
