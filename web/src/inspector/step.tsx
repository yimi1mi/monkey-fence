// Step Inspector(T8b):语义字段(instructions/agentInstance)与策略;
// 语义改动走 semantic CAS(阻止陈旧 Run);Observer 禁写。
export interface StepInspectorProps {
  step: { id: string; title: string; instructions: string; agentInstanceId: string };
  isController: boolean;
  onSemanticChange: (patch: { instructions?: string; agentInstanceId?: string }) => void;
}

export function StepInspector({ step, isController, onSemanticChange }: StepInspectorProps) {
  const disabled = !isController;
  return (
    <form aria-label={`步骤 ${step.title} 配置`}>
      <label>
        工作说明
        <textarea
          value={step.instructions}
          disabled={disabled}
          onChange={(e) => onSemanticChange({ instructions: e.target.value })}
        />
      </label>
      <label>
        Agent Instance
        <select
          value={step.agentInstanceId}
          disabled={disabled}
          onChange={(e) => onSemanticChange({ agentInstanceId: e.target.value })}
        >
          <option value={step.agentInstanceId}>{step.agentInstanceId}</option>
        </select>
      </label>
    </form>
  );
}
