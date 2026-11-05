import { timeStr } from './format'

export function candleOption(items: any[], showZoom = true) {
  const category = items.map((c) => timeStr(Math.floor(c.t / 1000), false))
  const ohlc = items.map((c) => [c.o, c.c, c.l, c.h])
  const volumes = items.map((c: any) => ({
    value: c.v,
    itemStyle: { color: c.c >= c.o ? 'rgba(18,196,139,0.45)' : 'rgba(240,75,90,0.45)' },
  }))
  return {
    animation: false,
    grid: [
      { left: 62, right: 16, top: 16, height: '58%' },
      { left: 62, right: 16, top: '76%', height: '14%' },
    ],
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'cross' },
      backgroundColor: '#0d1727',
      borderColor: '#1f3557',
      textStyle: { color: '#dbe6f5', fontSize: 11 },
    },
    xAxis: [
      {
        type: 'category',
        data: category,
        axisLine: { lineStyle: { color: '#1a2942' } },
        axisLabel: { color: '#6b7c99', fontSize: 9 },
        splitLine: { show: false },
      },
      {
        type: 'category',
        gridIndex: 1,
        data: category,
        axisLabel: { show: false },
        axisLine: { lineStyle: { color: '#1a2942' } },
        axisTick: { show: false },
      },
    ],
    yAxis: [
      { scale: true, splitLine: { lineStyle: { color: 'rgba(26,41,66,0.5)' } }, axisLabel: { color: '#6b7c99', fontSize: 10 } },
      { gridIndex: 1, splitLine: { show: false }, axisLabel: { show: false } },
    ],
    dataZoom: showZoom ? [{ type: 'inside', xAxisIndex: [0, 1], start: 55, end: 100 }] : [],
    series: [
      {
        type: 'candlestick',
        data: ohlc,
        itemStyle: { color: '#12c48b', color0: '#f04b5a', borderColor: '#12c48b', borderColor0: '#f04b5a' },
      },
      { type: 'bar', xAxisIndex: 1, yAxisIndex: 1, data: volumes },
    ],
  }
}
