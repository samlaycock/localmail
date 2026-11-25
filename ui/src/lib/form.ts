/** A text field of a submitted form: "" when it's missing (or a file). */
export function formText(form: FormData, name: string) {
  const value = form.get(name)
  return typeof value === "string" ? value : ""
}
