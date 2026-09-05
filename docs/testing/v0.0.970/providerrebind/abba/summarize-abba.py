import json, statistics
from pathlib import Path
root=Path('/tmp/providerrebind-abba')
legs=[json.loads((root/f'{i}-{v}.json').read_text()) for i,v in enumerate('ABBA',1)]
summary={'binary_order':'A B B A','baseline_commit':'7694ef9cbd2fbbcedb24fee14dbf4b12b1c4cd39','profile':'release (fat LTO, 1 codegen unit)','comparison_rule':'candidate median <= baseline median + max(baseline MAD,candidate MAD); no outliers removed','measurement_accepted':all(x['measurement_accepted'] for x in legs),'correctness_passed':all(not x['correctness_failures'] for x in legs),'legs':[{'leg':f'{i}-{v}','load':x['load_one_minute'],'accepted':x['measurement_accepted'],'reasons':x['measurement_reasons'],'summary':x['summary'],'failures':x['correctness_failures']} for i,(v,x) in enumerate(zip('ABBA',legs),1)],'comparison':{}}
for shape in ['single','tool']:
    values={v:[row['wall_ms'] for tag,leg in zip('ABBA',legs) if tag==v for row in leg['samples'][shape]] for v in 'AB'}
    def stats(xs):
        med=statistics.median(xs)
        return {'count':len(xs),'median_ms':med,'mad_ms':statistics.median(abs(x-med) for x in xs)}
    if not values['A'] or not values['B']:
        summary['comparison'][shape]={'A_count':len(values['A']),'B_count':len(values['B']),'within_mad':False,'reason':'incomplete correctness run'}
        continue
    a,b=stats(values['A']),stats(values['B'])
    tolerance=max(a['mad_ms'],b['mad_ms'])
    delta=b['median_ms']-a['median_ms']
    summary['comparison'][shape]={'A':a,'B':b,'delta_ms':delta,'tolerance_ms':tolerance,'within_mad':delta<=tolerance}
summary['passed']=summary['measurement_accepted'] and summary['correctness_passed'] and all(x['within_mad'] for x in summary['comparison'].values())
(root/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps({k:v for k,v in summary.items() if k!='legs'},indent=2))
