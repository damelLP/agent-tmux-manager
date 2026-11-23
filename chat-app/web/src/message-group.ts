export type GroupRow = {author:string;at:number;day:string;boundary:boolean};

export function continuesMessage(previous:GroupRow|undefined,current:GroupRow):boolean {
  if(!previous||previous.boundary||current.boundary||previous.author!==current.author||previous.day!==current.day) return false;
  const gap=current.at-previous.at;
  return gap>=0&&gap<=300_000;
}
