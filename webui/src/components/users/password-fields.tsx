import * as React from "react"

import { Field, FieldError, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"

/** "New password" + "confirm" pair; reports the value only when both match. */
export function NewPasswordFields({
  idPrefix,
  disabled,
  onChange,
  label = "New password",
}: {
  idPrefix: string
  disabled?: boolean
  onChange: (password: string | null) => void
  label?: string
}) {
  const [pw, setPw] = React.useState("")
  const [confirm, setConfirm] = React.useState("")
  const mismatch = confirm.length > 0 && pw !== confirm

  function update(nextPw: string, nextConfirm: string) {
    setPw(nextPw)
    setConfirm(nextConfirm)
    onChange(nextPw && nextPw === nextConfirm ? nextPw : null)
  }

  return (
    <>
      <Field>
        <FieldLabel htmlFor={`${idPrefix}-pw`}>{label}</FieldLabel>
        <Input
          id={`${idPrefix}-pw`}
          type="password"
          autoComplete="new-password"
          required
          value={pw}
          onChange={(e) => update(e.target.value, confirm)}
          disabled={disabled}
        />
      </Field>
      <Field data-invalid={mismatch || undefined}>
        <FieldLabel htmlFor={`${idPrefix}-confirm`}>
          Confirm password
        </FieldLabel>
        <Input
          id={`${idPrefix}-confirm`}
          type="password"
          autoComplete="new-password"
          required
          value={confirm}
          onChange={(e) => update(pw, e.target.value)}
          aria-invalid={mismatch || undefined}
          disabled={disabled}
        />
        {mismatch && <FieldError>Passwords do not match</FieldError>}
      </Field>
    </>
  )
}
