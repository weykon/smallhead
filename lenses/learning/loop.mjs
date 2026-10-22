// This layer changes each request, without replacing the user's original input.
export async function apply(context, event, next) {
  if (event.type === 'prepare_model') {
    event.request.system += `\n# Learning Lens\n${context.config.approach}\nUse actual available tools to check sources when needed. Distinguish the learner's own words from your inference. Do not save inferred mastery as a fact. Answer simple questions directly; avoid a fixed lesson pipeline.`;
    for (const tool of event.request.tools) {
      if (tool.name === 'web_search' || tool.name === 'read') {
        tool.description += ' In learning tasks, use this to verify source passages before citing them.';
      }
    }
  }
  return next(event);
}
