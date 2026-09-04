import type React from "react";
// schema 驱动声明式表单(#87 之五):字段描述符 → 渲染 + 校验 + 收集。
// 现有表单(CreateWorkflow/AddProject)逐步迁移;新表单一律走这里。

export type FormField =
  | {
      kind: "text";
      id: string;
      label: string;
      placeholder?: string;
      required?: boolean;
      hint?: string;
      datalist?: string[];
    }
  | {
      kind: "select";
      id: string;
      label: string;
      options: Array<{ value: string; label: string }>;
      required?: boolean;
      hint?: string;
    };

export interface FormSchema {
  fields: FormField[];
}

export type FormValues = Record<string, string>;

export function initialValues(schema: FormSchema): FormValues {
  const values: FormValues = {};
  for (const field of schema.fields) {
    values[field.id] = field.kind === "select" ? (field.options[0]?.value ?? "") : "";
  }
  return values;
}

/** 校验:required 非空。返回错误映射(空 = 通过)。 */
export function validate(schema: FormSchema, values: FormValues): Record<string, string> {
  const errors: Record<string, string> = {};
  for (const field of schema.fields) {
    if (field.required && !values[field.id]?.trim()) {
      errors[field.id] = `${field.label}不能为空`;
    }
  }
  return errors;
}

/** 渲染(datalist id 去重挂载由调用方容器负责)。 */
export function renderFields(
  schema: FormSchema,
  values: FormValues,
  errors: Record<string, string>,
  onChange: (id: string, value: string) => void,
): React.ReactNode[] {
  return schema.fields.map((field) => (
    <div className="field" key={field.id}>
      <label htmlFor={field.id}>{field.label}</label>
      {field.kind === "select" ? (
        <select
          id={field.id}
          value={values[field.id] ?? ""}
          onChange={(event) => onChange(field.id, event.target.value)}
        >
          {field.options.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </select>
      ) : (
        <>
          <input
            id={field.id}
            list={field.datalist ? `${field.id}-list` : undefined}
            value={values[field.id] ?? ""}
            placeholder={field.placeholder}
            onChange={(event) => onChange(field.id, event.target.value)}
          />
          {field.datalist && (
            <datalist id={`${field.id}-list`}>
              {field.datalist.map((option) => (
                <option key={option} value={option} />
              ))}
            </datalist>
          )}
        </>
      )}
      {errors[field.id] ? (
        <span className="hint" style={{ color: "var(--bad)" }}>
          {errors[field.id]}
        </span>
      ) : (
        field.hint && <span className="hint">{field.hint}</span>
      )}
    </div>
  ));
}
