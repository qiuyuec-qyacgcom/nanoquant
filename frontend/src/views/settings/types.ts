export type Widget = 'switch' | 'number' | 'text' | 'seg' | 'select' | 'tags' | 'json' | 'roi'

export interface FieldRow {
  key: string
  label: string
  hint?: string
  unit?: string
  widget: Widget
  options?: [string, string][]
  min?: number
  max?: number
  step?: number
}

export interface SectionGroup {
  title: string
  hint?: string
  rows: FieldRow[]
}