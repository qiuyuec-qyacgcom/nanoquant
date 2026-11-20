export interface CommandItem {
  label: string
  path: string
  ico?: string
}

export function filterCommands<T extends CommandItem>(items: readonly T[], query: string): T[] {
  const needle = query.trim().toLowerCase()
  if (!needle) return [...items]
  return items.filter(
    (item) =>
      item.label.toLowerCase().includes(needle) ||
      item.path.toLowerCase().includes(needle),
  )
}