import defaultMdxComponents from 'fumadocs-ui/mdx';
import { Step, Steps } from 'fumadocs-ui/components/steps';
import { File, Files, Folder } from 'fumadocs-ui/components/files';
import { Tab, Tabs } from 'fumadocs-ui/components/tabs';
import type { MDXComponents } from 'mdx/types';
import type { ReactNode } from 'react';
import { gitConfig, repoUrl } from '@/lib/shared';

/** Link to a file or directory in this repository on GitHub. */
function RepoLink({ path, children }: { path: string; children?: ReactNode }) {
  const href = `${repoUrl}/tree/${gitConfig.branch}/${path}`;
  return <a href={href}>{children ?? <code>{path}</code>}</a>;
}

/** The repository slug, e.g. in shell snippets. */
function Repo() {
  return <code>{`${gitConfig.user}/${gitConfig.repo}`}</code>;
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    Step,
    Steps,
    File,
    Files,
    Folder,
    Tab,
    Tabs,
    RepoLink,
    Repo,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
