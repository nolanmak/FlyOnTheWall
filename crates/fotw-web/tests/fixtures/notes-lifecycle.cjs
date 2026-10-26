const fs = require('node:fs'), vm = require('node:vm'), path = require('node:path');

function fixture() {
  class Element {
    constructor(tag) {
      this.tag=tag;this.children=[];this.listeners={};this.value='';
      this.selectionStart=this.selectionEnd=0;this.isConnected=true;
    }
    appendChild(child) {this.children.push(child);return child;}
    setAttribute(key,value) {this[key]=value;}
    addEventListener(name,fn) {this.listeners[name]=fn;}
    contains(node) {return this===node || this.children.some(child=>child.contains(node));}
    focus() {}
    setSelectionRange(start,end) {this.selectionStart=start;this.selectionEnd=end;}
  }
  const events=new Map(),timers=new Map();let timer=0;
  const context=vm.createContext({
    document:{createElement:tag=>new Element(tag)},
    window:{addEventListener:(name,callback)=>events.set(name,callback)},
    setTimeout:callback=>{timers.set(++timer,callback);return timer;},
    clearTimeout:id=>timers.delete(id),
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname,'../../ui/notes.js'),'utf8'),context);
  return {notes:context.FOTWNotes,host:()=>new Element('div'),events};
}

const settle=async()=>{for(let i=0;i<8;i++)await Promise.resolve();};
module.exports={fixture,settle};
