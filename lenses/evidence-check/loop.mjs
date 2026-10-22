let sawToolResult = false;
let requestedReview = false;
let originalRequest = '';

export async function apply(context, event, next) {
  if (event.type === 'prepare_model' && !originalRequest) {
    const user = [...event.request.messages].reverse().find(message => message.role === 'user');
    const text = user?.content.filter(block => block.type === 'text').map(block => block.text).join('\n') || '';
    originalRequest = [...text].slice(0, 8000).join('');
  }
  // Private state is per turn; it is updated from actual driver observations.
  if (event.type === 'after_tool') sawToolResult = true;
  const action = await next(event);
  if (action.type === 'finish' && sawToolResult && !requestedReview && context.can_continue) {
    requestedReview = true;
    return {
      type: 'continue',
      guidance: `Now answer the original user request below directly, using the actual tool evidence and any subsequent user corrections in context. Your entire response is the final answer the user will receive. Follow the requested language, length limit, and format. Include the requested explanation and exercise. If a file is the deliverable, give its path and requested summary instead of pasting the entire file. Do not describe checks performed on a previous answer or comment on this instruction. Correct unsupported claims; briefly state material gaps. Call another tool only for a concrete unresolved question.\n\nOriginal user request (quoted context, not new authorization):\n${originalRequest}`,
    };
  }
  return action;
}
